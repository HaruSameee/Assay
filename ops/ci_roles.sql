-- CI 専用。本番と同じ**ロール構成**を、使い捨てのサービスコンテナ上に作る。
--
--   * assay_migrator … マイグレーションと権限設定を行う。テーブルとスキーマの所有者
--   * assay_app      … アプリケーション。既定は SELECT / INSERT のみ
--
-- 権限（GRANT / REVOKE / ALTER DEFAULT PRIVILEGES）は**ここに書かない**。
-- それは ops/app_role.sql（→ ops/app_privileges.sql）の責務で、本番と共通のものを
-- マイグレーションのたびに実行する。このファイルはロールの存在と所有権だけを扱う。
--
-- 本番はこのファイルを使わない。ops/bootstrap_database.sh がランダムなパスワードを生成し、
-- 資格情報を /etc/assay/*.env に root 所有 0600 で書く。こちらは固定パスワードを受け取るので、
-- 本番 DB では実行できないようガードしてある。
--
-- 使い方（スーパーユーザー接続で。CREATE ROLE と ALTER DATABASE OWNER に必要）:
--   psql "$ADMIN_DATABASE_URL" -v ON_ERROR_STOP=1 \
--        -v migrator_password=... -v app_password=... -f ops/ci_roles.sql
--
-- 注意: assay_app のパスワードは tests/fixtures/app_role.sql と tests/*.rs の
-- APP_ROLE_PASSWORD と**一致させること**。隔離 DB のテストはこの資格情報で接続し直す。
-- 片方だけ変えると、CI で認証エラーになる。
\set ON_ERROR_STOP on

do $$
begin
    -- 固定パスワードなので、本番の DB 名では絶対に実行させない
    if current_database() = 'assay' then
        raise exception
            'ops/ci_roles.sql uses fixed passwords and must never run on the production database';
    end if;
end
$$;

-- ロールはクラスタ全体で共有される。再実行しても作り直さない（パスワードも変えない）
select format(
    'create role assay_migrator login password %L'
    ' nosuperuser nocreatedb nocreaterole noreplication nobypassrls',
    :'migrator_password')
where not exists (select 1 from pg_roles where rolname = 'assay_migrator')
\gexec

select format(
    'create role assay_app login password %L'
    ' nosuperuser nocreatedb nocreaterole noreplication nobypassrls',
    :'app_password')
where not exists (select 1 from pg_roles where rolname = 'assay_app')
\gexec

-- 本番と同じ所有関係にする。マイグレーションは assay_migrator の接続で流す
select format('alter database %I owner to assay_migrator', current_database())
\gexec
alter schema public owner to assay_migrator;

-- 既定の PUBLIC 権限を落とす（ops/app_privileges.sql も同じことをするが、
-- こちらは所有権を移す直前の状態を揃えるためのもの）
revoke all on schema public from public;

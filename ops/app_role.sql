-- アプリ用ロールの用意（デプロイ手順で実行する。マイグレーションからは呼ばない）。
--
-- 前提:
--   * マイグレーションはテーブル所有者（マイグレーション用ロール）で実行済み
--   * このスクリプトは所有者（または superuser）で実行する
-- tests/fixtures/app_role.sql と同じ形。ここを変えたらあちらも合わせる。
--
-- 使い方:
--   psql "$DATABASE_URL" -v app_role=<ロール名> -v app_password=<パスワード> -f ops/app_role.sql
--
-- 注意:
--   * SELECT ... FOR UPDATE / LOCK TABLE は対象テーブルの UPDATE 権限を要求する。
--     アプリは formation 単位のアドバイザリロック（pg_advisory_xact_lock）で直列化しており、
--     このロールに UPDATE を与えてはならない
--   * ロールの再実行は冪等（既にあれば作らない。GRANT / REVOKE は何度実行しても同じ）

-- ロール（無ければ作る）
select format('create role %I login password %L', :'app_role', :'app_password')
where not exists (select 1 from pg_roles where rolname = :'app_role')
\gexec

-- この DB への接続
select format('grant connect on database %I to %I', current_database(), :'app_role')
\gexec
grant usage on schema public to :"app_role";

-- 追記専用4テーブル: 読みと追記だけ
grant select, insert on formations, formation_players, formation_events, match_reports to :"app_role";
grant usage on sequence formation_events_id_seq, match_reports_id_seq to :"app_role";
revoke update, delete on formations, formation_players, formation_events, match_reports from :"app_role";

-- 可変の参照データ
grant select, insert, update, delete on players, roles to :"app_role";

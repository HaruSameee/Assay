-- テスト用のアプリロール。追記専用4テーブルの UPDATE / DELETE を持たない。
-- ops/app_role.sql と同じ形にしてある。ここを変えたらあちらも合わせる。
--
-- ロールはクラスタ全体で共有されるので、既にあれば作らない。
-- 並行するテストが同時に作ろうとして片方が失敗しても無視する。
do $$
begin
    create role unite_app_test login password 'unite_app_test';
exception
    when duplicate_object or unique_violation then null;
end
$$;

-- このテスト DB への権限（DB ごとに付け直す）
do $$
begin
    execute format('grant connect on database %I to unite_app_test', current_database());
end
$$;
grant usage on schema public to unite_app_test;

-- 追記専用4テーブル: 読みと追記だけ
grant select, insert on formations, formation_players, formation_events, match_reports to unite_app_test;
grant usage on sequence formation_events_id_seq, match_reports_id_seq to unite_app_test;
revoke update, delete on formations, formation_players, formation_events, match_reports from unite_app_test;

-- 可変の参照データ
grant select, insert, update, delete on players, roles to unite_app_test;

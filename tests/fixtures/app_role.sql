-- Test cluster only. Grants come from ops/app_privileges.sql, not a duplicate.
--
-- ロールはクラスタ全体で共有されるので、既にあれば作らない。
-- 並行するテストが同時に作ろうとして片方が失敗しても無視する。
do $$
begin
    create role assay_app login password 'assay_app_test'
        nosuperuser nocreatedb nocreaterole noreplication nobypassrls;
exception
    when duplicate_object or unique_violation then null;
end
$$;

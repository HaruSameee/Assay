-- Shared by ops/app_role.sql and sqlx test fixtures. Run as the table creator.
-- Default: append-only. Extend ONLY the mutable-table whitelist below.
do $$
begin
    if current_user = 'assay_app' then
        raise exception 'apply privileges as the migrator, not assay_app';
    end if;
    if exists (
        select 1 from pg_roles where rolname = 'assay_app'
        and (rolsuper or rolcreatedb or rolcreaterole or rolreplication or rolbypassrls)
    ) or pg_has_role('assay_app', current_user, 'MEMBER') then
        raise exception 'assay_app must be unprivileged and separate from the migrator';
    end if;
    execute format('grant connect on database %I to assay_app', current_database());
end
$$;

revoke all on schema public from public;
revoke create on schema public from assay_app;
grant usage on schema public to assay_app;

-- Remove old grants too, so rerunning this also repairs the old default policy.
revoke all on all tables in schema public from public, assay_app;
grant select, insert on all tables in schema public to assay_app;
-- players: mutable reference data; Discord profile information can change.
grant update, delete on players to assay_app;
-- roles: mutable display names and presentation order; identifiers stay stable.
grant update, delete on roles to assay_app;
-- queue_entries: live queue state, not a log. Entries change settings and disappear on match or leave; the history lives in formations.
grant update, delete on queue_entries to assay_app;
-- queue_entry_roles: dependent rows of queue_entries. Role preferences are rewritten in place and cascade away on leave.
grant update, delete on queue_entry_roles to assay_app;
-- player_pool_images: mutable reference data, one row per player. Re-uploading replaces the image in place; it is not a log and nothing is derived from it.
grant update on player_pool_images to assay_app;
revoke all on all sequences in schema public from public, assay_app;
grant usage, select on all sequences in schema public to assay_app;

-- SQLx's ledger is infrastructure, not an application append-only table.
revoke all on _sqlx_migrations from assay_app;

-- Per-schema REVOKE cannot cancel global defaults, so clear BOTH scopes.
alter default privileges revoke all on tables from public, assay_app;
alter default privileges in schema public revoke all on tables from public, assay_app;
alter default privileges in schema public grant select, insert on tables to assay_app;
alter default privileges revoke all on sequences from public, assay_app;
alter default privileges in schema public revoke all on sequences from public, assay_app;
alter default privileges in schema public grant usage, select on sequences to assay_app;

-- Must return zero rows. Shared between CI and production verification.
with tables as (
    select c.oid, c.relname,
        c.relname in ('players', 'roles', 'queue_entries', 'queue_entry_roles') as mutable,
        has_table_privilege('assay_app', c.oid, 'SELECT') as can_select,
        has_table_privilege('assay_app', c.oid, 'INSERT') as can_insert,
        has_table_privilege('assay_app', c.oid, 'UPDATE') as can_update,
        has_any_column_privilege('assay_app', c.oid, 'UPDATE') as can_update_column,
        has_table_privilege('assay_app', c.oid, 'DELETE') as can_delete,
        has_table_privilege('assay_app', c.oid, 'TRUNCATE') as can_truncate,
        exists (
            select 1 from pg_trigger t
            where t.tgrelid = c.oid and not t.tgisinternal
                and t.tgfoid = 'public.forbid_update_delete'::regproc
                and t.tgenabled in ('O', 'A')
                -- BEFORE UPDATE OR DELETE, statement-level (also rejects zero rows).
                and t.tgtype = 26
        ) as protected
    from pg_class c join pg_namespace n on n.oid = c.relnamespace
    where n.nspname = 'public' and c.relkind in ('r', 'p')
        and c.relname <> '_sqlx_migrations'
)
select relname::text as violation from tables
where not can_select or not can_insert or can_truncate
    or (mutable and (not can_update or not can_delete or protected))
    or (not mutable and (can_update_column or can_delete or not protected))
union all
select 'assay_app: elevated role or schema ownership'
from pg_roles r where r.rolname = 'assay_app'
and (r.rolsuper or r.rolcreatedb or r.rolcreaterole or r.rolreplication or r.rolbypassrls
    or has_schema_privilege('assay_app', 'public', 'CREATE'))
union all
select '_sqlx_migrations: application access'
where has_table_privilege('assay_app', 'public._sqlx_migrations', 'SELECT,INSERT,UPDATE,DELETE,TRUNCATE');

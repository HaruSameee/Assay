-- Run as the migration/table owner after EVERY migration:
-- psql "$MIGRATOR_DATABASE_URL" -f ops/app_role.sql
-- assay_app must already exist. Role creation belongs to cluster provisioning.
\set ON_ERROR_STOP on
begin;
\ir app_privileges.sql
commit;

-- Compatibility entry point. Use the same fail-closed policy as every deployment.
-- Run as the migrator; the application role is assay_app.
\set ON_ERROR_STOP on
\ir app_role.sql

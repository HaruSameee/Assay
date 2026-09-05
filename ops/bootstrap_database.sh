#!/usr/bin/env bash
# Initial provisioning only. Never rotates existing credentials or replaces a DB.
set -euo pipefail
umask 077
test "$(id -u)" = 0
if test -e /etc/assay/app-database.env || test -e /etc/assay/migrator-database.env; then
    echo 'Assay credentials already exist; refusing to overwrite.' >&2
    exit 1
fi
existing=$(runuser -u postgres -- psql -XAt -v ON_ERROR_STOP=1 -d postgres -c \
    "select count(*) from pg_roles where rolname in ('assay_app', 'assay_migrator')")
database=$(runuser -u postgres -- psql -XAt -v ON_ERROR_STOP=1 -d postgres -c \
    "select count(*) from pg_database where datname = 'assay'")
test "$existing" = 0 && test "$database" = 0 || {
    echo 'Assay roles or database already exist; inspect them before proceeding.' >&2
    exit 1
}
install -d -m 700 /etc/assay
migrator_password=$(openssl rand -hex 32)
app_password=$(openssl rand -hex 32)
# Save first so a later failure cannot lose credentials for a created role.
printf 'MIGRATOR_DATABASE_URL=postgresql://assay_migrator:%s@127.0.0.1:5432/assay?sslmode=disable\n' "$migrator_password" > /etc/assay/migrator-database.env
printf 'DATABASE_URL=postgresql://assay_app:%s@127.0.0.1:5432/assay?sslmode=disable\n' "$app_password" > /etc/assay/app-database.env
runuser -u postgres -- psql -X -v ON_ERROR_STOP=1 -d postgres <<SQL
begin;
create role assay_migrator login password '$migrator_password'
    nosuperuser nocreatedb nocreaterole noreplication nobypassrls;
create role assay_app login password '$app_password'
    nosuperuser nocreatedb nocreaterole noreplication nobypassrls;
commit;
create database assay owner assay_migrator;
\connect assay
alter schema public owner to assay_migrator;
revoke all on schema public from public;
SQL
unset migrator_password app_password
echo 'Created assay and separate login roles. Credentials: /etc/assay/*-database.env (root, 0600).'

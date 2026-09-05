# Assay backup and restore rehearsal

The production host is in UTC. The cron schedule in `assay-backup.cron` runs
the Friday/Saturday/Sunday 21:00--01:00 JST window as 12:00--16:00 UTC, and
the weekly full snapshot at Monday 02:15 JST (Sunday 17:15 UTC).

Each snapshot has this R2 object prefix:

    <R2_PREFIX>/<YYYYMMDDTHHMMSSZ-hourly|weekly>/

It contains:

- `assay.dump`: custom-format database dump, including ACLs and ownership.
- `roles.sql`: `pg_dumpall --roles-only` output, including the database roles.
- `SHA256SUMS`: checksum manifest for both files.

The scripts load `/etc/assay/r2-backup.env` at runtime. It must remain root
owned and mode 0600. Do not put its values on a command line, in a shell
history, or in this document.

## Create and inspect a snapshot

Run a manual snapshot before enabling cron or before a recovery rehearsal:

    /usr/local/sbin/assay-backup weekly

The command prints only the generated snapshot ID. To list remote snapshots,
source the R2 environment in a root shell and run the S3 client with the R2
endpoint. Do not echo the environment values.

## Restore rehearsal

Run the following as root, substituting the snapshot ID printed by the backup
command:

    /usr/local/sbin/assay-restore-rehearsal 20260907T171500Z-weekly

The script creates a separate PostgreSQL 17 cluster on a Unix socket and port
55432. It does not use, alter, or stop the production cluster. The exact order
is:

1. Download the R2 snapshot and verify `SHA256SUMS`.
2. Initialize a new cluster and restore `roles.sql` as its fresh-cluster
   superuser.
3. Create database `assay` owned by `assay_migrator`, then restore
   `assay.dump`.
4. Run `ops/app_role.sql` as `assay_migrator`.
5. Run `ops/check_app_role.sql` as `assay_app`; it must return zero rows.
   This is the same privilege/trigger audit embedded by `tests/schema.rs`.
6. Run `ops/verify_app_role.sql` as `assay_app`; it verifies append-only
   UPDATE and DELETE are rejected with SQLSTATE 42501.

The target cluster and downloaded role dump are removed on completion, even on
failure. Successful output is:

    restore-audit=passed
    append-only-update=42501

## Deployment

Install `awscli`, the scripts, the cron file, and logrotate configuration via
the release artifact procedure in `deploy.md`. Install `/etc/assay/r2-backup.env`
once by copying `r2-backup.env.example` and setting its values manually. Never
overwrite the configured file during a deployment.

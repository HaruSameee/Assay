# Assay deployment

This procedure deploys the release-build artifact without installing Rust or
sqlx-cli on the VPS. Run every command as root or through sudo. Never print
or copy the contents of /etc/assay/*.env.

## One-time host setup

1. Create the unprivileged service account and installation directories:

        useradd --system --user-group --home-dir /opt/assay --shell /usr/sbin/nologin assay
        install -d -o root -g root -m 0755 /opt/assay/bin /opt/assay/ops
        install -o root -g root -m 0600 ops/app.env.example /etc/assay/app.env.example

2. Install Caddy before installing its configuration:

        apt-get update
        apt-get install -y awscli caddy

3. Install the artifact's ops/assay.service as
   /etc/systemd/system/assay.service, then run:

        systemctl daemon-reload
        systemctl enable assay.service

4. Install ops/Caddyfile as /etc/caddy/Caddyfile, validate it with caddy
   validate --config /etc/caddy/Caddyfile, then enable and restart
   caddy.service.

## Each deployment

1. Download the unite-matchsystem-<commit> artifact from the successful
   release-build GitHub Actions run. Verify the bundled SHA-256 file before
   installation:

        sha256sum -c unite-matchsystem.sha256
        if test -x /opt/assay/bin/unite-matchsystem; then
          install -o root -g root -m 0755 \
            /opt/assay/bin/unite-matchsystem \
            /opt/assay/bin/unite-matchsystem.prev
        fi
        install -o root -g root -m 0755 unite-matchsystem /opt/assay/bin/unite-matchsystem
        install -o root -g root -m 0644 ops/app_role.sql ops/app_privileges.sql \
          ops/check_app_role.sql ops/verify_app_role.sql ops/app.env.example \
          ops/r2-backup.env.example ops/restore.md /opt/assay/ops/
        install -o root -g root -m 0750 ops/assay-backup ops/assay-restore-rehearsal \
          /usr/local/sbin/
        install -o root -g root -m 0644 ops/assay-backup.cron /etc/cron.d/assay-backup
        install -o root -g root -m 0644 ops/assay-backup.logrotate /etc/logrotate.d/assay-backup
        install -o root -g root -m 0600 ops/r2-backup.env.example /etc/assay/r2-backup.env.example
        install -d -o postgres -g postgres -m 0700 /var/lib/postgresql/assay-restore-rehearsals
        touch /var/log/assay-backup.log
        chown root:root /var/log/assay-backup.log
        chmod 0600 /var/log/assay-backup.log

2. Apply migrations only through the embedded migration command with the
   migrator credential. The command does not start Discord or the HTTP server:

        set -a
        . /etc/assay/migrator-database.env
        set +a
        /opt/assay/bin/unite-matchsystem migrate
        psql "$MIGRATOR_DATABASE_URL" -v ON_ERROR_STOP=1 -f /opt/assay/ops/app_role.sql
        unset MIGRATOR_DATABASE_URL

3. Verify the app role without displaying its credential, then restart:

        set -a
        . /etc/assay/app-database.env
        set +a
        psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f /opt/assay/ops/verify_app_role.sql
        test -z "$(psql "$DATABASE_URL" -XAt -v ON_ERROR_STOP=1 -f /opt/assay/ops/check_app_role.sql)"
        unset DATABASE_URL
        systemctl restart assay.service
        systemctl --no-pager --full status assay.service

4. Confirm the service resolved every configured VC channel and that the
   reverse proxy is healthy. Scope the log to the restart you just performed:
   -b covers the whole machine boot, so it accumulates every earlier restart
   and the count will not match the pool size.

        invocation="$(systemctl show -p InvocationID --value assay.service)"
        journalctl _SYSTEMD_INVOCATION_ID="$invocation" --no-pager \
          | grep 'voice channel pair resolved'
        journalctl _SYSTEMD_INVOCATION_ID="$invocation" --no-pager \
          | grep -c 'voice channel pair resolved'
        grep '^DISCORD_VC_POOL_SIZE=' /etc/assay/app.env
        curl --fail --show-error https://match.aynm.dev/healthz

   The count must equal DISCORD_VC_POOL_SIZE. The service refuses to start when
   a pair is missing or ambiguous, so a short count means you are reading the
   wrong invocation, not a partial pool.

   If the grep finds nothing, check the log line before concluding the pool is
   broken. The wording lives in src/discord/mod.rs and has changed once already
   (it was "resolved voice channel" until 2026-09-06). Do not roll back a
   running service on a failed grep alone.

5. Remove the plaintext copies of the application environment once the checks
   above pass. Each edit of /etc/assay/app.env leaves one behind, and every one
   of them holds the bot token, the OAuth client secret and the session key:

        ls /etc/assay/app.env.bak-*
        shred -u /etc/assay/app.env.bak-*

   Keep /etc/assay/app.env itself. Roll back the binary, not the environment —
   the recovery path below does not read these files.

If the new binary must be rolled back, restore the immediately preceding binary
and restart the service:

        test -x /opt/assay/bin/unite-matchsystem.prev
        install -o root -g root -m 0755 \
          /opt/assay/bin/unite-matchsystem.prev \
          /opt/assay/bin/unite-matchsystem
        systemctl restart assay.service
        systemctl --no-pager --full status assay.service

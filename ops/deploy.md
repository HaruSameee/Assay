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
        apt-get install -y caddy

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
        install -o root -g root -m 0755 unite-matchsystem /opt/assay/bin/unite-matchsystem
        install -o root -g root -m 0644 ops/app_role.sql ops/app_privileges.sql \
          ops/check_app_role.sql ops/verify_app_role.sql ops/app.env.example /opt/assay/ops/

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
   reverse proxy is healthy:

        journalctl -u assay.service -b --no-pager | grep 'resolved voice channel'
        curl --fail --show-error https://match.aynm.dev/healthz

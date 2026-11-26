# SharpNode WAF + Splunk stack

Minimal WAF + SIEM stack for the HoneNode host. The WAF is inline in front of
the app; Splunk receives WAF, broker and app logs through the HEC. The host
runs Fluent Bit.

## Layout

```text
deploy/security-stack/
├── docker-compose.yml          # WAF + Splunk
├── .env.example                # copy to .env and set strong values
├── waf/sharpnode-rules.conf    # SharpNode-specific ModSecurity rules
├── waf/modsecurity-override.conf.template
├── splunk/apps/sharpnode/      # Splunk app: dashboard, indexes, props
└── fluent-bit/fluent-bit.conf  # host shipper: journal + WAF logs -> HEC
```

## Host prerequisites

- HoneNode app listening on `127.0.0.1:8088`.
- Podman (or Docker).
- `podman-compose` installed with pip; Rocky 10 does not package it.
- Fluent Bit from the Fluent Bit repository or the `fluent/fluent-bit` container.

## 1. Prepare the host

```sh
sudo mkdir -p /srv/waf/logs /srv/waf/rules /srv/splunk/apps
sudo cp waf/sharpnode-rules.conf waf/modsecurity-override.conf.template /srv/waf/rules/
sudo cp -a splunk/apps/sharpnode /srv/splunk/apps/
sudo chown -R "$USER":"$(id -gn)" /srv/waf/logs /srv/waf/rules /srv/splunk/apps
sudo chmod 0750 /srv/waf/logs
sudo chmod 0644 /srv/waf/rules/*
```

The stack runs rootless. The WAF image runs as the unprivileged `nginx` user
(uid 101), which rootless Podman would normally map to a subordinate host uid.
The compose file sets `userns_mode: keep-id:uid=101,gid=101`, so container uid
101 maps to your login user; the host paths must therefore be owned by your
login user. The `z` flag relabels them for SELinux.

The config files must also live outside your home directory: `/home/<user>` is
`0700` and `user_home_t`, so the container cannot read them and the entrypoint
exits with status 2. The same applies to the Splunk app, which is why it is
copied to `/srv/splunk/apps`.

If the WAF image's nginx UID is not `101`, check it and adjust `userns_mode`:

```sh
podman run --rm owasp/modsecurity-crs:nginx id -u nginx
```

## 2. Configure secrets

```sh
cp .env.example .env
# edit .env and set SPLUNK_PASSWORD and SPLUNK_HEC_TOKEN
chmod 0600 .env
```

Do not commit `.env`.

## 3. Start the stack

Rocky 10 does not package `podman-compose`; install it with pip:

```sh
sudo dnf install -y python3-pip
python3 -m pip install --user podman-compose
~/.local/bin/podman-compose --version
```

Then, from `deploy/security-stack`:

```sh
~/.local/bin/podman-compose up -d
~/.local/bin/podman-compose ps
```

If `pip` refuses because the environment is externally managed, use `pipx`:

```sh
sudo dnf install -y pipx
pipx install podman-compose
pipx ensurepath
podman-compose up -d
```

## 4. Test the WAF

```sh
curl -i -H 'X-Lab-Bloquear: si' http://127.0.0.1:8080/
```

Expected: `HTTP/1.1 403`. The WAF proxies normal requests to the app on
`127.0.0.1:8088` and listens on `127.0.0.1:8080`.

The WAF starts with `MODSEC_RULE_ENGINE=On`. If the OWASP CRS blocks a
legitimate app form, switch to `DetectionOnly` in `docker-compose.yml`, inspect
`/srv/waf/logs/audit.log`, add an exclusion, then switch back to `On`.

## 5. Open Splunk

Splunk is bound to loopback. From the Fedora lab:

```sh
ssh -L 18000:127.0.0.1:8000 node1@192.168.0.106
```

Open `http://127.0.0.1:18000` and sign in with the `SPLUNK_PASSWORD` from
`.env`. The **SharpNode Security** app contains the dashboard. Use `18000`
locally if port `8000` is already taken on the Fedora machine.

The HEC is on `127.0.0.1:18088` (mapped from the container's 8088, because the
app already uses 8088 on the host).

## 6. Install Fluent Bit

Option A — Fluent Bit repository:

```sh
sudo tee /etc/yum.repos.d/fluent-bit.repo <<'EOF'
[fluent-bit]
name=Fluent Bit
baseurl=https://packages.fluentbit.io/rockylinux/10/$basearch/
gpgcheck=1
gpgkey=https://packages.fluentbit.io/fluentbit.key
enabled=1
EOF
sudo dnf install -y fluent-bit
sudo chgrp fluent-bit /srv/waf/logs
sudo cp fluent-bit/fluent-bit.conf /etc/fluent-bit/fluent-bit.conf
sudo sed -i 's/REPLACE_WITH_SPLUNK_HEC_TOKEN/<your-hec-token>/' /etc/fluent-bit/fluent-bit.conf
sudo chmod 0600 /etc/fluent-bit/fluent-bit.conf
sudo usermod -aG systemd-journal fluent-bit
sudo systemctl enable --now fluent-bit
sudo journalctl -u fluent-bit -n 50 --no-pager
```

Option B — container (no package):

```sh
sudo mkdir -p /etc/fluent-bit /var/lib/fluent-bit
sudo cp fluent-bit/fluent-bit.conf /etc/fluent-bit/fluent-bit.conf
sudo sed -i 's/REPLACE_WITH_SPLUNK_HEC_TOKEN/<your-hec-token>/' /etc/fluent-bit/fluent-bit.conf
sudo chmod 0600 /etc/fluent-bit/fluent-bit.conf
podman run -d --name sharpnode-fluent-bit --network host --user root \
  -v /etc/fluent-bit/fluent-bit.conf:/fluent-bit/etc/fluent-bit.conf:ro \
  -v /srv/waf/logs:/srv/waf/logs:ro,z \
  -v /var/log/journal:/var/log/journal:ro \
  -v /run/systemd/journal:/run/systemd/journal:ro \
  -v /var/lib/fluent-bit:/var/lib/fluent-bit \
  fluent/fluent-bit:latest
```

The shipper reads:

- `honed.service` and `honenode.service` from the journal → `index=honenode`
- `/srv/waf/logs/audit.log` → `index=waf`, `sourcetype=waf_audit`
- `/srv/waf/logs/access.log` → `index=waf`, `sourcetype=waf_access`
- `/srv/waf/logs/error.log` → `index=waf`, `sourcetype=waf_error`

The Caddy input is commented out until Caddy is installed.

## 7. Dashboard

The Splunk app is mounted read-only at `/opt/splunk/etc/apps/sharpnode`. It
creates the `waf`, `honenode` and `caddy` indexes and the
**SharpNode Security → Overview** dashboard:

- WAF events, blocked count, broker requests
- WAF actions over time (area chart)
- Top source IPs (pie chart)
- Recent WAF blocks
- Broker activity

The dashboard uses the dark theme plus `sharpnode.css` for the gold/black
SharpNode palette.

## 8. Caddy later

When Caddy is installed, change the WAF `BACKEND` to the Caddy loopback
listener (or place the WAF behind Caddy), and enable the Caddy input in
`fluent-bit/fluent-bit.conf`. The WAF's default `PROXY_HOST_HEADER` (`$host`)
already preserves the original Host.

## Troubleshooting

```sh
podman logs sharpnode-waf
podman logs sharpnode-splunk
sudo tail -n 50 /srv/waf/logs/audit.log
sudo journalctl -u fluent-bit -n 50 --no-pager   # package install
podman logs sharpnode-fluent-bit                 # container install
```

- **`open /srv/waf/logs: permission denied` when starting the WAF:** the host
  paths are not owned by the login user. Re-run the `chown` from step 1; the
  `keep-id` mapping requires `/srv/waf` to be owned by your login user.
- **WAF container exits with status 2:** the entrypoint could not read a
  bind-mounted file. Check `podman logs sharpnode-waf` for
  `cannot open ... Permission denied`; make sure the files are in
  `/srv/waf/rules` (not in your home directory) and that the compose mounts
  carry the `z` flag.
- **WAF 403 on legitimate requests:** switch to `DetectionOnly`, inspect the
  audit log, add an exclusion, switch back to `On`.
- **No WAF events in Splunk:** check the Fluent Bit token, the HEC port
  `18088`, and that the `fluent-bit` user can read `/srv/waf/logs`.
- **No broker events in Splunk:** check that the `fluent-bit` user is in the
  `systemd-journal` group and restart Fluent Bit.
- **Splunk dashboard empty:** confirm the `waf` and `honenode` indexes exist
  and the searches use the same sourcetypes as the Fluent Bit filters.

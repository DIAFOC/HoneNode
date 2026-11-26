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
├── splunk/apps/sharpnode/      # Splunk app: dashboard, indexes, props
└── fluent-bit/fluent-bit.conf  # host shipper: journal + WAF logs -> HEC
```

## Host prerequisites

- HoneNode app listening on `127.0.0.1:8088`.
- Podman (or Docker) and `podman-compose`.
- Fluent Bit on the host.
- The `fluent-bit` group exists (created by the Fluent Bit package).

## 1. Prepare the host

```sh
sudo mkdir -p /srv/waf/logs /srv/waf/rules
sudo chown 101:fluent-bit /srv/waf/logs
sudo chmod 0750 /srv/waf/logs
sudo cp waf/sharpnode-rules.conf /srv/waf/rules/
```

If the WAF image's nginx UID is not `101`, check it and adjust the owner:

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

```sh
podman-compose up -d
podman-compose ps
```

If `podman-compose` is missing:

```sh
sudo dnf install podman-compose
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
ssh -L 8000:127.0.0.1:8000 node1@192.168.0.106
```

Open `http://127.0.0.1:8000` and sign in with the `SPLUNK_PASSWORD` from
`.env`. The **SharpNode Security** app contains the dashboard.

The HEC is on `127.0.0.1:18088` (mapped from the container's 8088, because the
app already uses 8088 on the host).

## 6. Install Fluent Bit

```sh
sudo dnf install fluent-bit
sudo cp fluent-bit/fluent-bit.conf /etc/fluent-bit/fluent-bit.conf
sudo sed -i 's/REPLACE_WITH_SPLUNK_HEC_TOKEN/<your-hec-token>/' /etc/fluent-bit/fluent-bit.conf
sudo chmod 0600 /etc/fluent-bit/fluent-bit.conf
sudo usermod -aG systemd-journal fluent-bit
sudo systemctl enable --now fluent-bit
sudo journalctl -u fluent-bit -n 50 --no-pager
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
`fluent-bit/fluent-bit.conf`. The WAF's `SERVER_NAME` and `PROXY_HOST_HEADER`
already preserve the original Host.

## Troubleshooting

```sh
podman logs sharpnode-waf
podman logs sharpnode-splunk
sudo tail -n 50 /srv/waf/logs/audit.log
sudo journalctl -u fluent-bit -n 50 --no-pager
```

- **WAF 403 on legitimate requests:** switch to `DetectionOnly`, inspect the
  audit log, add an exclusion, switch back to `On`.
- **No WAF events in Splunk:** check the Fluent Bit token, the HEC port
  `18088`, and that the `fluent-bit` user can read `/srv/waf/logs`.
- **No broker events in Splunk:** check that the `fluent-bit` user is in the
  `systemd-journal` group and restart Fluent Bit.
- **Splunk dashboard empty:** confirm the `waf` and `honenode` indexes exist
  and the searches use the same sourcetypes as the Fluent Bit filters.

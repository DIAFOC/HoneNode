# HoneNode — backend

> **Evaluation status (2026-11-23):** the workspace is labeled **0.8.8
> experimental**. Real Fedora tests have demonstrated native tenant
> web/database/storage/git provisioning and generic registry/image provisioning
> with environment configuration. This is not a production release or a
> Rocky/SELinux certification. The SharpNode reference deployment scripts live
> outside this repository; they are not the HoneNode installer. See
> `packaging/os/README.md` for the OS-tagged evaluation bundles.

> **Fedora acceptance update (2026-11-10):** tenant web/database creation through
> the real browser UI has been exercised on this workstation. Operator pages
> remain neutral and tenant pages use SharpNode. See
> [`packaging/fedora/ACCEPTANCE.md`](../packaging/fedora/ACCEPTANCE.md) for evidence,
> observed defects, and the required installation of the compiled broker fixes.
> The historical status notes below are not a claim that 0.1.0 is released.

Two binaries, one workspace.  The split is the most important design decision in
the project (§3.4), so it is visible in the layout:

| Crate | Runs as | Authority |
|---|---|---|
| `crates/honenode` | rootless container, distroless | **none** — no Podman socket, no tenant sockets, no shell |
| `crates/honed` | host systemd service | the **only** component with systemd/root authority |

The app writes *desired state* to the SQLite registry; the broker reconciles:
render the unit atomically → validate → `daemon-reload` → start/stop.

Licence: **GPL-3.0-or-later**.  See `../LICENSING.md`.

## What is implemented

**`honenode`** — the management plane:

* SQLite registry with the schema of §4.4 plus `tenants.slug` and `sessions`,
  WAL mode, foreign keys on, parameterised queries only, migrations embedded in
  the binary;
* authentication by account number (12 digits, Luhn check digit, rejection
  sampling) and `argon2id`, with server-side sessions and per-session CSRF
  tokens compared in constant time;
* operator dashboard (tenants, services, jobs, audit, host telemetry) and tenant
  portal (own services, lifecycle, links, activity), server-rendered with
  `askama` templates and a static CSS asset. The current dark theme is the
  SharpNode reference theme; HoneNode 0.1.0 requires a neutral default and
  tenant self-service provisioning from the portal;
* the service catalogue, plans, additive time and the grace → suspend → purge
  lifecycle, swept in the background;
* telemetry from cgroup v2, `/proc/pressure/*` and `statvfs`;
* structured audit events, ECS-inspired, always on;
* a token-authenticated JSON API.

**`honed`** — the broker:

* the Unix socket contract at `0660 root:sharpnode`, with peer credentials in
  the audit trail;
* typed arguments only: unknown operations, malformed tenant ids, tenant ids the
  broker does not know, and unit names that do not belong to the tenant are all
  refused before anything else happens;
* `tenant.create` / `tenant.delete` (account, subuid/subgid, linger, data dir,
  internal network), `limits.set` (renders the Quadlet unit from the plan
  envelope), `service.start|stop|restart|suspend|resume|delete`, `logs.fetch`,
  and `edge.reload` (renders the vhost and reloads Caddy through its loopback
  admin API).

**Deliberately not implemented:** `db.console`.  The broker answers
`not_implemented` — never a silent success.  A mediated database console needs
its own design before it ships.

## Build

Rootless, as the platform account, so the image lands in the same storage as
everything else:

```sh
sudo deploy/06-app-build.sh          # stages, builds both artefacts, installs honed
```

Or by hand:

```sh
cargo build --release -p honed
podman build -t localhost/honenode-app:dev -f backend/Containerfile backend/
```

The views live in `crates/honenode/templates/` (HTML) and the stylesheet in
`static/hone.css` (served at `/static/hone.css`).  Askama embeds the templates
at compile time; the CSS is copied into the runtime image and served by the app,
so design iteration does not require editing Rust source.

## Run locally (development)

```sh
# broker — needs a writable socket path and a tenant that exists on the host
HONE_BROKER_SOCKET=/tmp/honed.sock HONE_TENANTS=t01 cargo run -p honed &

# app
HONE_BROKER_SOCKET=/tmp/honed.sock HONE_REGISTRY=/tmp/registry.db \
HONE_OPERATOR_PASSWORD=dev-only HONE_LOG_FORMAT=plain cargo run -p honenode
```

The operator account number is generated on first boot and written to the log:

```sh
journalctl --user-unit honenode-app | grep 'operator account created'
```

Then `curl http://127.0.0.1:8080/api/v1/broker` to prove the socket contract.

## Conventions

* **No comments in the source.**  The reasoning lives in
  `Texla/HoneNode_Memorias_Tecnicas.tex` and in this file; the code carries
  names, types and structure instead.
* **Parameterised queries only.**  Values travel as bound parameters, never
  interpolated into the query text.
* **Typed arguments only** across the socket.  Never a path, never a raw string,
  never interpolation into `Exec=`, `Volume=` or `Environment=`.
* **Every privileged action is audited** with actor + tenant + outcome + reason.
* The registry has **no network surface at all**.
* HTML views are template files, not Rust macros.  CSS is a static asset.  The
  CSP allows no inline styles or scripts; keep it that way.

## Deviations recorded

* `sqlx::query!` (compile-time-checked SQL) is not used yet: the runtime API
  with bound parameters gives the same injection defence without requiring a
  database at build time.  Migrating to the macros is a CI task, with the
  `.sqlx` metadata committed.
* Per-tenant disk accounting is broker-mediated, because the platform account
  cannot read the tenants' `0750` data directories — and should not be able to.

# HoneNode backend

Two binaries, one Rust workspace. The authority split is the central design
decision:

| Crate | Binary | Runs as | Authority |
|---|---|---|---|
| `crates/honenode` | `honenode` | rootless container, distroless | **none** — no Podman socket, no tenant sockets, no shell |
| `crates/honed` | `honed` | host systemd service | the **only** component with systemd/root authority |

The app owns the SQLite registry and writes *desired state*. The broker owns
admission, unit rendering, secrets and service lifecycle. The app never talks to
Podman or systemd directly.

## Layout

- `crates/honenode/` — axum app: registry and migrations, account/session auth,
  operator and tenant pages, JSON API, catalogue and provisioning requests.
  - `templates/` — askama HTML views.
  - `tests/` — offline Python checks for templates, scripts and SQL migrations.
- `crates/honed/` — root broker: Unix-socket contract, policy/capacity admission,
  Quadlet rendering, secret files, service lifecycle and generic containers.
- `static/` — CSS and the single external browser script (`hone.js`).
- `Containerfile` — multi-stage app image; runtime layer is distroless.
- `Cargo.lock` — committed. CI, the release build and the image build use
  `--locked`.

The SharpNode host wizard, lab reset helper and demo-tenant material are **not**
part of this repository; do not look for them under `deploy/` or
`packaging/fedora/`. The OS bundles and their examples are the interface for
this repo.

## Build and test

```sh
cargo test --all-targets --all-features
cargo build --release --locked -p honed -p honenode
python3 -m unittest discover -s crates/honenode/tests
podman build -t localhost/honenode-app:dev -f Containerfile .
```

A release artifact is built by `packaging/os/fedora/build.sh`,
`packaging/os/rocky/build.sh` or the `release.yml` workflow. Users do not need
a Rust toolchain to run a published app image or OS bundle.

## Run locally

The broker is a host component; it needs a host-like environment with systemd
and rootless Podman. For a quick app-only development loop, point it at a
broker socket that already exists:

```sh
HONE_BROKER_SOCKET=/tmp/honed.sock HONE_TENANTS=t01 cargo run -p honed &
HONE_BROKER_SOCKET=/tmp/honed.sock HONE_REGISTRY=/tmp/registry.db \
  HONE_OPERATOR_PASSWORD=dev-only cargo run -p honenode
```

On first boot without `HONE_OPERATOR_PASSWORD`, the app generates the operator
credentials once, logs them and writes a mode-0600 bootstrap file next to the
registry. Save them and delete the file.

The repeatable end-to-end acceptance is
`bash packaging/rocky/run_smoke_podman.sh`; it boots Rocky 10 and exercises the
real broker/app path. See `../packaging/README.md`.

## Invariants

- Parameterised SQL only; values are bound, never interpolated.
- Typed broker requests only: no paths, raw shell, or unit directives from the
  app. Unknown fields are rejected.
- Secrets are redacted in `Debug`, zeroed on drop, and never written to jobs,
  audit rows, logs, templates or the app registry. Initial File Browser and
  PostgreSQL passwords are accepted from the deployment wizard and applied only
  when the broker creates the unit for the first time.
- The app has no host authority; the broker is the only privileged component.
- `db.console` is deliberately not implemented. The broker returns
  `not_implemented` rather than a silent success.
- CSP allows no inline scripts or styles; `/static/hone.js` is the only script.
- `sqlx::query!` compile-time checked SQL is not used yet; the runtime API with
  bound parameters gives the same injection defence without a build-time DB.
  Migrating to the macros with committed `.sqlx` metadata remains a CI task.

Licence: **GPL-3.0-or-later**. See `../LICENSE`.

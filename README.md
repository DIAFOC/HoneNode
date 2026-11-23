# HoneNode

[![CI](https://github.com/DIAFOC/HoneNode/actions/workflows/ci.yml/badge.svg)](https://github.com/DIAFOC/HoneNode/actions/workflows/ci.yml)
[![Release](https://github.com/DIAFOC/HoneNode/actions/workflows/release.yml/badge.svg)](https://github.com/DIAFOC/HoneNode/actions/workflows/release.yml)

**HoneNode 0.8.8 — experimental single-host multi-tenant hosting plane.**

HoneNode is the reproducible management plane of a SharpNode deployment: a
Rust app (`honenode`) plus a root broker (`honed`) that owns rootless Podman
Quadlet units for per-tenant web, database, storage, Git and generic-container
services. The app never has host authority; the broker is the only component
with it.

The host deployment scripts, the SharpNode documentation and the demo tenant
material live in the SharpNode repository, not here. This repository is the app
source, its packaging and the CI/security pipeline.

## Layout

- `backend/` — Rust workspace: `crates/honenode` (app), `crates/honed`
  (broker), static assets, the distroless app `Containerfile`.
- `packaging/os/` — OS-tagged Fedora/Rocky evaluation bundles.
- `packaging/rpm/` — host-agent-only RPM prototype.
- `packaging/oci/` — direct OCI archive / local preview tooling.
- `packaging/rocky/` — repeatable Rocky 10 smoke test.
- `packaging/release/` — historical Rocky evaluation bundle builder.
- `packaging/tests/` — offline packaging tests.
- `security/` — STRIDE threat model and the custom Semgrep rules.
- `.github/workflows/` — CI, security analysis, DAST and release automation.

## Build and test

```sh
cargo build --release --manifest-path backend/Cargo.toml
cargo test --manifest-path backend/Cargo.toml
python3 -m unittest discover -s backend/crates/honenode/tests
python3 -m unittest discover -s packaging/tests
```

## OS-tagged bundles

```sh
bash packaging/os/fedora/build.sh   # Fedora toolchain
bash packaging/os/rocky/build.sh    # boots Rocky 10 and builds there
```

Each bundle records `"os"` and the ELF libc/interpreter in `manifest.json`.
Do not install the Fedora bundle on Rocky or vice versa.

## CI and security

- `CI` (`ci.yml`): Rust format/clippy/tests, Python tests, Cargo Audit, Cargo
  Deny, Semgrep SAST, Trivy filesystem analysis, Gitleaks, OSV-Scanner,
  ShellCheck, Hadolint, Actionlint, ZAP baseline DAST, and advisory OpenSCAP
  and Lynis scans. Findings are uploaded as SARIF where the tool supports it.
- The STRIDE model and the threat-to-tool mapping are in `security/`.
- Host-wide OpenSCAP/Lynis and the nested Rocky smoke are advisory on GitHub:
  the runner is not the target host, and nested systemd/Podman is best effort.

## Release

Push a tag:

```sh
git tag v0.8.8
git push origin v0.8.8
```

`release.yml` builds the Fedora and Rocky bundles and the app container image,
then creates the GitHub release `v0.8.8` with those assets. `workflow_dispatch`
can produce the same release manually with the `version` input.

## License

GPL-3.0-or-later. See `LICENSE`.

# HoneNode — Fedora evaluation bundle

This bundle is built on Fedora and contains Fedora-built ELF binaries. It is
tagged `"os": "fedora"` in `manifest.json`; do not install it on Rocky or on a
host with an older glibc. Use the Rocky bundle for Rocky.

## Contents

- `native/honenode` — the management plane (app)
- `broker/honed` — the host broker
- `native/static/` — the app's static assets
- `examples/` — policy, environment and systemd examples
- `README.md`, `manifest.json`, `SHA256SUMS`

## Build

```sh
bash packaging/os/fedora/build.sh
```

The script builds with the Fedora toolchain, tags the bundle as Fedora, and
runs the Rust and Python suites unless `--skip-tests` is passed. In CI the same
script runs inside `fedora:latest` and the resulting archive is attached to the
GitHub release.

## Quick install

Extract the bundle and run the installer as root:

```sh
sudo ./install.sh --dry-run
sudo ./install.sh --yes
```

The installer verifies `manifest.json`/`SHA256SUMS`, installs the native app and
broker, creates the `honenode` system user/group and directories, installs the
self-service policy (`t01..t06`), starts both systemd units on loopback, and
prints the operator credentials once. The app image is also published to GHCR;
the host broker remains a native systemd component.

## Runtime

The bundle contains the app and broker binaries; `install.py` is the host
integration step. The Fedora lab wizard is separate and lives in the SharpNode
host repository; it is not required for a clean bundle install.

The app starts with `HONE_BIND`, `HONE_BROKER_SOCKET`, `HONE_REGISTRY`,
`HONE_STATIC_DIR` and the policy/tenant environment. On first boot it generates
the operator credentials once (journal plus a mode-0600 bootstrap file).

## Verification

```sh
cargo test --manifest-path backend/Cargo.toml
python3 -m unittest discover -s backend/crates/honenode/tests
python3 -m unittest discover -s packaging/tests
```

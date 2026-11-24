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

## Runtime

The bundle contains the app and broker binaries; it does not install a host
service, create tenants, or start anything. Host integration is manual in
0.8.8: review the examples, install the broker unit and policy, and start the
app from the extracted tree. Use the OS-specific bundle with the matching host.

The app starts with `HONE_BIND`, `HONE_BROKER_SOCKET`, `HONE_REGISTRY`,
`HONE_STATIC_DIR` and the policy/tenant environment. On first boot it generates
the operator credentials once (journal plus a mode-0600 bootstrap file).

## Verification

```sh
cargo test --manifest-path backend/Cargo.toml
python3 -m unittest discover -s backend/crates/honenode/tests
python3 -m unittest discover -s packaging/tests
```

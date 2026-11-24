# HoneNode — Rocky Linux evaluation bundle

This bundle is built inside Rocky Linux 10 and contains Rocky-built ELF
binaries. It is tagged `"os": "rocky"` in `manifest.json`; do not install it on
Fedora. Use the Fedora bundle there.

## Contents

- `native/honenode` — the management plane (app)
- `broker/honed` — the host broker
- `native/static/` — the app's static assets
- `examples/` — policy, environment and systemd examples
- `README.md`, `manifest.json`, `SHA256SUMS`

## Quick install

Extract the bundle and run the installer as root:

```sh
sudo ./install.sh --dry-run
sudo ./install.sh --yes
```

The installer verifies `manifest.json`/`SHA256SUMS`, installs the native app and
broker, creates the `honenode` system user/group and directories, installs the
self-service policy (`t01..t06`), starts both systemd units on loopback, and
prints the operator credentials once. Use `--tenants t91,t92` to change the
allowlist. The app image is also published to GHCR, but the host broker remains
a native systemd component.

## Host installation

This bundle ships the broker binary (`broker/honed`), the app binary
(`native/honenode`), and example units/policy/environment under `examples/`.
It does **not** install a systemd unit, a policy, tenants, or SELinux labels by
itself: `install.py` performs that host step after you review the defaults.
The experimental RPM in the source tree is a separate prototype and is not an
asset of this bundle.

For an evaluation host, review `examples/systemd/`, `examples/policy.example.json`
and `examples/app.env.example`, install the broker unit and policy as the root
operator, then start the app from the extracted `native/` directory. Host
integration and SELinux readiness remain operator tasks.

## Evaluation smoke

The repeatable acceptance test boots Rocky 10 with systemd and exercises the
real broker/app path:

```sh
bash packaging/rocky/run_smoke_podman.sh
```

After building this bundle, test the released binaries instead of a source
rebuild:

```sh
bash packaging/os/rocky/build.sh
bash packaging/rocky/run_smoke_podman.sh --prebuilt
```

The CI workflow builds the Rocky binaries once, uploads them, and runs the full
smoke with `--prebuilt`. The smoke provisions the fixed nginx template, writes
site content through the panel, provisions a second nginx and a second
PostgreSQL, and re-provisions the web service after deletion. It also runs the
full signup/key/redemption flow.

## Verification

```sh
python3 -m unittest discover -s packaging/tests
```

The Rocky container installs Rust and builds from source, so the smoke tests
the Rocky-built binaries rather than a Fedora artifact.

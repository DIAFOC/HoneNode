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

## Host installation

The experimental RPM remains the host packaging path:

```sh
sudo rpm -i honed-<version>-<release>.x86_64.rpm
```

The RPM installs only the broker and its systemd unit. It does not install,
enable, or start the app, and it installs no tenant allowlist or active policy.
Host integration and SELinux readiness remain operator tasks.

## Evaluation smoke

The repeatable acceptance test boots Rocky 10 with systemd and exercises the
real broker/app path:

```sh
bash packaging/rocky/run_smoke_podman.sh
```

The smoke provisions the fixed nginx template, writes site content through the
panel, provisions a second nginx and a second PostgreSQL, and re-provisions the
web service after deletion. It also runs the full signup/key/redemption flow.

## Verification

```sh
python3.14 -m unittest discover -s packaging/tests
```

The Rocky container installs Rust and builds from source, so the smoke tests
the Rocky-built binaries rather than a Fedora artifact.

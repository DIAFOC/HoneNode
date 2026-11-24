# HoneNode packaging

This directory builds the **evaluation artifacts**, including the bundle
installer. The SharpNode lab wizard (`packaging/fedora/`) is intentionally not
in this repository.

## What ships

| Artifact | Built from | Contains | How it is run |
|---|---|---|---|
| App OCI image | `backend/Containerfile`; `packaging/oci/` for the direct archive | `honenode`, static assets, distroless runtime | `podman load` + `podman run` (app only; needs a broker for provisioning) |
| Fedora bundle | `packaging/os/fedora/build.sh` | native `honenode` + `honed`, static tree, `install.sh`/`install.py`, examples, `manifest.json`, per-OS README | extract on Fedora; `sudo ./install.sh` |
| Rocky bundle | `packaging/os/rocky/build.sh` | native `honenode` + `honed`, static tree, `install.sh`/`install.py`, examples, `manifest.json`, per-OS README | extract on Rocky 10; `sudo ./install.sh` |
| RPM prototype | `packaging/rpm/` | **only** `honed` and its systemd integration | experimental, RHEL 10-gated, not a release asset |
| Smoke | `packaging/rocky/` | acceptance flow | boots Rocky 10 and drives the real broker/app path |

The GitHub `release.yml` workflow builds the Fedora bundle, the Rocky bundle
and the app image on a `v*` tag, attaches the bundles and the offline image tar
to the release, and publishes the app image to GHCR. The RPM prototype is not
published by that workflow.

## Build the bundles

```sh
bash packaging/os/fedora/build.sh   # on Fedora, with the native toolchain
bash packaging/os/rocky/build.sh    # boots Rocky 10 and builds there
```

Both write to `packaging/os/dist/` and record `"os"`, the architecture and the
ELF libc/interpreter in `manifest.json`. Do not install the Fedora bundle on
Rocky or the Rocky bundle on Fedora.

See [`os/README.md`](os/README.md) for the bundle layout and per-OS READMEs.

## What a user actually needs

- **The app is precompiled.** The OCI image and the OS bundles contain the app;
  users do not need Rust or Cargo.
- **The broker is a host component.** It needs systemd and rootless Podman, so
  it is not containerized. The OS bundle's `install.py` installs the broker and
  app units, creates the socket group and directories, installs the self-service
  policy and prints the first-run credentials.
- **The setup wizard is `install.sh`/`install.py`.** It verifies the bundle, uses the
  prebuilt native binaries, defaults to a loopback panel and tenants `t01..t06`,
  and supports `--dry-run`, `--tenants`, `--bind`, `--force` and `--uninstall`.
  The `examples/` files remain for review and manual overrides.
- **The smoke tests the built artifacts.** CI builds the Rocky bundle once, and
the full smoke consumes those binaries with `--prebuilt` rather than
  recompiling. Locally:
  `bash packaging/os/rocky/build.sh && bash packaging/rocky/run_smoke_podman.sh --prebuilt`.

## Reproducibility and current limits

- `Cargo.lock` is committed and the app image builds with `--locked`.
- The OS bundles are built inside their target distro containers with that
  distro's toolchain; the manifests record the target.
- The app image builder is pinned to `rust:1.99-alpine`; the runtime layer is
  `distroless/static-debian12:nonroot`. The tags are pinned, but not yet by
  digest. Pinning digests and adding SBOM/provenance is release-hardening work,
  not a 0.8.8 claim.
- SELinux policy, a signed artifact pipeline, a Fedora full smoke on GitHub,
  and a Fedora installer acceptance remain open. The bundle installer is a first
  working path, not a Fedora/Rocky certification. See
  [`../security/README.md`](../security/README.md) and the root README.

## Offline tests

```sh
python3 -m unittest discover -s packaging/tests
```

The suite covers bundle structure and OS tags, OCI archive construction,
rootless preview confinement, the RPM spec, and the release builder. Subprocesses
are mocked; these tests do not install packages or start containers.

Historical material and the older Rocky evaluation builder are under
`packaging/release/`; `packaging/oci/README.md` documents the local preview path.

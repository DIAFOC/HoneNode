# HoneNode packaging

This directory builds the **evaluation artifacts**. It is not a whole-product
installer, and `packaging/fedora/` (the SharpNode lab wizard) is intentionally
not in this repository.

## What ships

| Artifact | Built from | Contains | How it is run |
|---|---|---|---|
| App OCI image | `backend/Containerfile`; `packaging/oci/` for the direct archive | `honenode`, static assets, distroless runtime | `podman load` + `podman run` (app only; needs a broker for provisioning) |
| Fedora bundle | `packaging/os/fedora/build.sh` | native `honenode` + `honed`, static tree, examples, `manifest.json`, per-OS README | extract on Fedora; host integration is manual |
| Rocky bundle | `packaging/os/rocky/build.sh` | native `honenode` + `honed`, static tree, examples, `manifest.json`, per-OS README | extract on Rocky 10; host integration is manual |
| RPM prototype | `packaging/rpm/` | **only** `honed` and its systemd integration | experimental, RHEL 10-gated, not a release asset |
| Smoke | `packaging/rocky/` | acceptance flow | boots Rocky 10 and drives the real broker/app path |

The GitHub `release.yml` workflow builds the Fedora bundle, the Rocky bundle
and the app image on a `v*` tag, then attaches those three assets to the
release. The RPM prototype is not published by that workflow.

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
  it is not containerized. The OS bundle ships the native `honed` binary and
  example units; installing the unit, creating the socket group, installing a
  reviewed policy and enabling tenants remain operator actions in 0.8.8.
- **The examples are not an installer.** `examples/app.env.example`,
  `examples/policy.example.json` and `examples/systemd/` show the required
  shape. A one-command installer is still deliberately absent: it would imply
  SELinux, policy, capacity and credential decisions had been made for the
  operator.
- **The smoke now tests the built binaries.** CI builds the Rocky binaries in
  the `rocky-build` job, uploads them, and the full smoke runs with
  `--prebuilt`; it does not recompile inside the target container. Locally:
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
  and a one-command host install remain open. See
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

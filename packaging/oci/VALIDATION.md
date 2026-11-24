# Fedora OCI preview validation — 2026-11-10 (historical)

This file records an app-only preview validation performed during 0.8.x. It is
not a release certification and not a claim of live tenant provisioning. The
current acceptance path is the Rocky smoke plus the CI suites described in the
root README and `packaging/README.md`.

## Outcome

The rootless `honenode-preview` container was resumed and left running at
`http://127.0.0.1:8088/` (container port 8080). The original state and
credentials were preserved in `packaging/.local-run/bootstrap.env`; that file
contains secrets and must be read only in a private terminal.

This was **app-only preview acceptance**: no host broker mount, no real tenant
provisioning, and no change to host accounts, root services, firewall or SELinux
policy.

## Evidence from that run

| Check | Result |
|---|---|
| Offline, locked musl release build of `honenode` and `honed` | Passed; existing artifacts up to date |
| Static ELF linkage | No `INTERP` or `DT_NEEDED`; actual app execution also passed |
| OCI archive descriptors | Manifest/config/layer sizes and SHA-256 digests verified |
| Loaded image | Image ID equals the verified archive config digest |
| Rust workspace tests | 60 broker + 36 app passed (historical counts) |
| Packaging Python tests | 40 passed (historical count) |
| App SQL/theme Python tests | 16 SQL + 9 theme passed (historical counts) |
| Isolated Playwright workflow | Passed with a simulated broker |
| Real OCI Playwright smoke | Passed using the saved bootstrap |
| Real preview restart | Authenticated session, database inode and bootstrap preserved |
| Repeated health checks | 100 requests in batches of 10 passed |
| SQLite `PRAGMA quick_check` after restart | `ok`; one operator, no tenants |

Real-container observations at the time: rootless Podman, numeric `65532:65532`,
read-only root filesystem, no privileged mode or host networking,
`no-new-privileges`, `--cap-drop ALL`, enforcing SELinux (`container_t`), no
`label=disable`, 512 MiB memory/swap, one CPU, 256 PID ceiling, only the private
preview data tree bind-mounted, and HTTP published only on `127.0.0.1:8088`.

## Reproduce

Run from the repository root with the local toolchain and Playwright
installation you already have:

```sh
cargo test --manifest-path backend/Cargo.toml --all-targets --all-features
python3 -m unittest discover -s packaging/tests -v
python3 -m unittest discover -s backend/crates/honenode/tests -v

# Optional browser checks; require Playwright and a running preview.
PLAYWRIGHT_BROWSERS_PATH=backend/.build-tools/browser-cache \
  node packaging/tests/browser_smoke.cjs
PLAYWRIGHT_BROWSERS_PATH=backend/.build-tools/browser-cache \
  node packaging/tests/preview_smoke.cjs --restart
```

Build the static archive with `packaging/oci/build_archive.py` as documented in
`README.md`; Askama templates are embedded, so future source changes require a
rebuild.

## Artifact identity (that historical run only)

```text
App binary SHA-256:
a216cd9a23862aa5590b9d7661a3383d13f4628554745352b3eeb0082d09a2b9
OCI archive SHA-256:
f6faf1f9072dd24f2efa1b10e42cf8626fff70da281be0e9f0834289cbf075e1
OCI config / loaded image SHA-256:
6a7dd3e4c59ea1f3e7f68ff2ecd92b4453e3ccba8bb9b34be16fac1966471bdb
```

These are not release hashes and must not be used to verify a published 0.8.8
asset.

## Still outside the preview result

- A one-command host installation (broker unit, policy, socket group, tenants).
- Reviewed SELinux policy for broker/app/socket integration.
- A Fedora full smoke on GitHub (the Rocky full smoke is the current E2E).
- Signed artifacts, SBOM/provenance and a digest-pinned build base image.
- Sustained-load, upgrade/uninstall and credential-rotation testing.

The Fedora lab wizard and its material live in the SharpNode host repository,
outside this app source tree. Do not infer that a successful preview authorizes
or completes a host deployment.

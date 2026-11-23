# Fedora OCI preview validation — 2026-11-10

## Outcome

The interrupted app-preview validation is complete. The existing rootless
`honenode-preview` container was found stopped with exit code 0, resumed, and
left running at **http://127.0.0.1:8088/** (container port 8080).

The original state and credentials were preserved. Sign in using
`HONE_OPERATOR_ACCOUNT` and `HONE_OPERATOR_PASSWORD` from
`packaging/.local-run/bootstrap.env`; view that file only in a private terminal.
The successful login used those saved credentials, not a generated replacement.

This is **app-only preview acceptance**, not HoneNode 0.1.0 release acceptance.
There is no host broker mount, no real tenant provisioning, and no change to
host accounts, root services, firewall, or SELinux policy.

## Fresh checks

| Check | Result |
|---|---|
| Offline, locked musl release build of `honenode` and `honed` | Passed; existing artifacts up to date |
| Static ELF linkage | No `INTERP` or `DT_NEEDED`; actual app execution also passed |
| OCI archive descriptors | Manifest/config/layer sizes and SHA-256 digests verified |
| Archive contents | App binary equals current release artifact; complete static tree equals source |
| Loaded image | Image ID equals verified archive config digest |
| Rust workspace tests | 60 broker + 36 app passed |
| Packaging Python tests | 40 passed |
| Fedora lab Python tests | 18 passed; mocked/sandbox checks, not host installation |
| App SQL/theme Python tests | 16 SQL + 9 theme passed |
| Isolated Playwright workflow | Passed with simulated broker |
| Real OCI Playwright smoke | Passed using saved bootstrap |
| Real preview restart | Existing authenticated session, database inode and bootstrap preserved |
| Repeated health checks | 100 requests in batches of 10 passed |
| SQLite `PRAGMA quick_check` after restart | `ok`; one operator, no tenants |

The SQL regression suite initially failed during import because it extracted
an INSERT from `create_service`, which now delegates to `reserve_service`.
`backend/crates/honenode/tests/test_registry_sql.py` now uses the production
reservation query and its 16 bindings. Coverage includes cross-plan quotas,
failed reservations, tombstones, tenant isolation, concurrent writers, and
retained port/volume reservations. No production code was changed in this
resumption. The release build still reports 15 existing app dead-code warnings.

### Real-container evidence

- Local rootless Podman; numeric app user `65532:65532`.
- Read-only root filesystem, no privileged mode or host networking,
  `no-new-privileges`, and launch-time `--cap-drop ALL`.
- Enforcing SELinux with a `container_t` process label; no `label=disable`.
- 512 MiB memory/swap ceiling, one CPU, 256 PID ceiling.
- Only the private preview data tree bind-mounted; no Podman or broker socket.
- HTTP published only on `127.0.0.1:8088`.
- Health, CSP, `nosniff`, unauthenticated operator denial, disabled API-token
  endpoints, saved-credential login, all tested operator routes, and CSS passed.
- Resource snapshot after restart: about 1.7 MiB of 512 MiB memory, seven PIDs.
  This is a point-in-time observation, not a sustained-load benchmark.

### Isolated workflow evidence

`packaging/tests/browser_smoke.cjs` launched the real static app as a temporary
native process on port 18089 with a simulated broker. It checked neutral
operator pages, SharpNode tenant pages, operator-created tenant login,
catalogue provisioning of web and database services, stop/start, exhausted
quota UI, tenant denial from the operator surface, and mobile viewport bounds.
Screenshots were regenerated under `packaging/.local-run/screenshots/`.
They depict fixture services, not running host containers.

## Reproduce

Run from the repository root. The toolchain and browser paths below refer to
this workstation's already installed local build tools; no download is needed.

```sh
env PATH=/home/cos/Documents/DD5/backend/.build-tools/cargo/bin:/usr/bin:/bin \
  CARGO_HOME=/home/cos/Documents/DD5/backend/.build-tools/cargo \
  RUSTUP_HOME=/home/cos/Documents/DD5/backend/.build-tools/rustup \
  cargo test --manifest-path backend/Cargo.toml --workspace --offline --locked

python3 -B -m unittest discover -s packaging/tests -v
python3 -B -m unittest discover -s packaging/fedora/tests -v
python3 -B -m unittest discover -s backend/crates/honenode/tests -v

PLAYWRIGHT_BROWSERS_PATH=backend/.build-tools/browser-cache \
  node packaging/tests/browser_smoke.cjs

# Requires the existing running app-only preview. Explicitly restarts it.
PLAYWRIGHT_BROWSERS_PATH=backend/.build-tools/browser-cache \
  node packaging/tests/preview_smoke.cjs --restart
```

Static build command used successfully:

```sh
env PATH=/home/cos/Documents/DD5/backend/.build-tools/musl/bin:/usr/bin:/bin \
  CARGO_HOME=/home/cos/Documents/DD5/backend/.build-tools/cargo \
  RUSTUP_HOME=/home/cos/Documents/DD5/backend/.build-tools/rustup \
  CC_x86_64_unknown_linux_musl=/home/cos/Documents/DD5/backend/.build-tools/musl/bin/musl-gcc \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=/home/cos/Documents/DD5/backend/.build-tools/rustup/toolchains/stable-x86_64-unknown-linux-gnu/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld \
  backend/.build-tools/cargo/bin/cargo build --manifest-path backend/Cargo.toml \
  --release --locked --offline --target x86_64-unknown-linux-musl -p honenode -p honed
```

The archive did not need rebuilding: its embedded app and static assets exactly
match those current artifacts. Use the builder in `README.md` after future
source changes; Askama templates are embedded and require recompilation.

## Artifact identity

```text
App binary SHA-256:
a216cd9a23862aa5590b9d7661a3383d13f4628554745352b3eeb0082d09a2b9
OCI archive SHA-256:
f6faf1f9072dd24f2efa1b10e42cf8626fff70da281be0e9f0834289cbf075e1
OCI config / loaded image SHA-256:
6a7dd3e4c59ea1f3e7f68ff2ecd92b4453e3ccba8bb9b34be16fac1966471bdb
```

## Still outside this result

- Real app-to-host broker access, reviewed socket DAC/SELinux integration,
  rootless tenant managers and actual web/database service acceptance.
- Storage/Git provisioning: the broker currently offers web/database only;
  app fixture tests for additional kinds do not establish broker support.
- Production setup/credential rotation, TLS/listener separation, policy and
  capacity review, disk quotas, RPM installation/upgrade, signatures/SBOM,
  and long-duration or realistic-load testing.

Use `packaging/fedora/README.md` for the separate administrative host-lab path.
Do not infer that a successful preview authorizes or completes that deployment.

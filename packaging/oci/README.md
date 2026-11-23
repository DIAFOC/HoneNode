# Local Fedora app preview (no Docker or Buildah CLI)

This is a **standalone app/server preview**, not a host installer or a live
provisioning deployment. It can show the app and bootstrap sign-in even when
`honed` is not installed. The scripts do not install packages, modify host
`/etc`, create host tenant accounts, start root services, or configure Caddy.
No Dockerfile/Containerfile is used by this path.

## Requirements

- Python 3.10+ (stdlib only), `readelf` from binutils, and a **prebuilt static
  Linux `honenode` ELF** for the host architecture (x86-64 or AArch64).
- Local **rootless Podman** with `keep-id:uid=65532,gid=65532` support, configured
  subordinate UID/GID ranges, a working rootless network, and delegated cgroup
  v2 memory/CPU/PID controls. Current Fedora Podman/crun is the intended runtime;
  tests here do not certify a Fedora runtime version. The launcher refuses root,
  a rootful engine, and remote Podman connections.
- An SELinux-capable runtime: enforcement stays on. Only the dedicated app data
  directory receives a private `:Z` label. Do not relabel arbitrary host paths.

The builder needs **neither Podman nor Buildah**, does not compile, does not
pull a base image, and does not access a container daemon. The existing
`build_image.py` remains a separate, optional legacy Buildah path.

## Exact commands, from the repository root

After obtaining the static artifact, pass its actual path with `--binary`.
For example, **if** your separately completed build produced the usual musl
target path below:

```sh
python3 -B packaging/oci/build_archive.py \
  --binary backend/target/x86_64-unknown-linux-musl/release/honenode \
  --static-dir backend/static \
  --image localhost/honenode-app:poc \
  --output packaging/honenode-app.oci.tar
```

The musl path is an example, **not a claim that this target was built or works**.
Use an AArch64 artifact on an AArch64 host. The builder obtains image architecture
from the ELF, not the build host. It reuses `build_image.verify_static_elf`:
`readelf -hW`, `-lW`, and `-dW` must show ELF with no interpreter and no
`DT_NEEDED` libraries. A dynamic binary fails before archive creation; do not
remove the gate or put it in scratch anyway. A static glibc artifact may still
have NSS, resolver, locale, and `dlopen` caveats; test the actual binary.

**Only after the static build/archive succeeds**, the operator may launch:

```sh
python3 -B packaging/oci/run_local.py --archive packaging/honenode-app.oci.tar
```

This loads with `podman load --input`, then requests one **detached** container
named `honenode-preview`. It does not wait forever, start a foreground server,
replace an existing container, or infer HTTP health from a container ID.
Alternatively, load explicitly and launch an already loaded tag:

```sh
podman load --input packaging/honenode-app.oci.tar
python3 -B packaging/oci/run_local.py --image localhost/honenode-app:poc
```

No network pulls are allowed by the run command. For a different tag, pass the
same `--image localhost/name:tag` to both scripts. No static artifact or actual
Podman load/run is exercised by the packaging tests.

If the image is already loaded, omit `--archive`; loading the same archive
again is optional. The launcher explicitly sets `HONE_THEME=sharpnode` for
 tenant/default pages even when an older loaded image specifies `neutral`.
The app itself forces operator pages to neutral; this is not a tenant theme
setting. No archive rebuild is needed for these launcher overrides.

### Check the preview, then sign in

```sh
podman ps -a --filter name=honenode-preview
podman logs --tail 80 honenode-preview
curl --fail --max-time 10 http://127.0.0.1:8088/
```

Open **http://127.0.0.1:8088/** locally. View the initial credentials only in a
private terminal (the launcher does not print them):

```sh
cat packaging/.local-run/bootstrap.env
```

Use `HONE_OPERATOR_ACCOUNT` as the sign-in identifier, and
`HONE_OPERATOR_PASSWORD` as the password. The file contains secrets; do not
paste it into issue reports or commit it. Local plain HTTP requires
`HONE_SECURE_COOKIES=false`; the override is deliberate for this loopback-only
preview, **not suitable for a public listener or production**. The published
socket is always `127.0.0.1:8088:8080`, not all host interfaces. Port 8088 must be
free. No TLS or firewall change is made.

### State and bootstrap

The script anchors state to `packaging/.local-run/`, independent of the current
working directory:

```text
.local-run/                  mode 0700, owned by the invoking user
  bootstrap.env              mode 0600, operator account + password
  data/                      mode 0700, private SELinux relabel allowed here only
    registry/                SQLite database and WAL/SHM persist here
    tenants/                 app-side preview data, not host tenant storage
```

Without a supplied bootstrap, Python `secrets` generates a 32-byte URL-safe
password and eleven random account digits plus the app's Luhn check digit.
Credentials are written exclusively (no overwrite), validated, and reused on
subsequent runs. The app only uses them when its registry has no operator;
changing the file does **not** reset an existing operator password. A missing
bootstrap alongside an existing database is an error rather than a misleading
new password. Keep database and bootstrap backups together. The script refuses
symlink state/credential paths and existing directories/files with unsafe
ownership or modes; it never recursively chowns a host directory.

To supply your own bootstrap, create a user-owned, regular mode-`0600` UTF-8
file in a private directory and use:

```sh
python3 -B packaging/oci/run_local.py \
  --archive packaging/honenode-app.oci.tar \
  --bootstrap-env /absolute/private/path/bootstrap.env
```

Supported lines are raw, **unquoted** `HONE_OPERATOR_ACCOUNT=value` and/or
`HONE_OPERATOR_PASSWORD=value`; this is not a shell file and is never sourced.
Blank lines and comments beginning with `#` are allowed. Unknown/duplicate keys,
control characters, invalid account numbers, or passwords outside 12–1024
UTF-8 bytes are rejected. Omitted values are generated once. Supplied values
must agree with an existing saved bootstrap; restart is not credential rotation.
Secrets are passed via `--env-file`, not command-line values. As with ordinary
Podman environment files, the same user/root can inspect the container's
environment; this is not a hardware-backed secret store.

### Container boundaries

The launcher uses:

- `podman run -d --pull never --read-only --read-only-tmpfs=false`;
- `--userns keep-id:uid=65532,gid=65532 --user 65532:65532`, so the persistent
  bind is writable as your host user without `:U`, root chown, or world access;
- `--cap-drop ALL --security-opt no-new-privileges`;
- `--pids-limit 256 --memory 512m --memory-swap 512m --cpus 1` (no fallback to
  silently dropping failed limits; review cgroup delegation if Podman fails);
- explicit size-limited, `nosuid,nodev,noexec` tmpfs mounts for `/tmp` and the
  private `/run/honenode` directory. The latter is root-owned mode `0755`, so the
  nonroot app can traverse it to look for the absent socket but cannot write
  there. No tmpfs `uid=`/`gid=` options are used (Podman 6.1 rejects them);
- only `packaging/.local-run/data:/var/lib/honenode:rw,Z` by default, plus the
  private bootstrap env file, **not** `/etc`, a Podman/Docker socket, or a host
  tenant tree. No `--privileged`, host networking, or `label=disable`.

The archive contains the binary, **all** static files recursively (including
nested directories), and empty state/runtime directories. Static traversal
rejects symlinks (including directory/dangling symlinks), devices, FIFOs and
sockets; descriptor-relative, no-follow opens avoid following replaced links.
Only normalized regular files/directories are stored, never host ownership,
setuid bits, or symlink/hardlink tar entries. Files have read-only image modes;
state belongs to numeric UID/GID 65532, `/tmp` is sticky, and `/var/lib/honenode`
is declared as a volume. The layer is uncompressed; config `rootfs.diff_ids`
uses its SHA-256. All config/manifest/layer descriptors carry SHA-256 and sizes,
and `index.json` carries the local tag. Archive output is replaced atomically
only on success; its parent must already exist and be outside the static tree.

### Broker is absent by default

`HONE_BROKER_SOCKET=/run/honenode/broker.sock` points into a **private empty
container directory** by default. The app's tenant list is `t91,t92`, reserved
for lab review; the builder's default image tenant list remains empty. These
settings **do not create tenant accounts or authorize a broker**. No host
`/run/honenode` mount is attempted automatically, even if it happens to exist.
The app can listen and serve a preview without a broker, but broker-dependent
status/provisioning operations can fail. **Provisioning is not live.**

Only after separate host integration review may an operator explicitly opt in:

```sh
python3 -B packaging/oci/run_local.py --broker-dir /run/honenode
```

The directory must contain a real `broker.sock` Unix socket. It is bound
read-only **without relabelling**, with `--group-add keep-groups` (requires
crun) for an already authorized user's supplementary groups. A read-only bind
does **not** make a broker API read-only: authorized calls may change the host.
Access still depends on directory/socket DAC, user namespaces and a reviewed
SELinux policy. Do not add `label=disable`, `--privileged`, or mount the Podman
socket to bypass failures. Explicit mounting proves neither connection nor
policy/capacity/provisioning readiness. Broker and app data paths also need
review before any integrated deployment; this preview does not expose host
tenant storage.

Installing/configuring a root broker is a **separate, manual administrative
action**. This launcher never invokes sudo or the host installer, and does not
retry `sudo -n`. In a Fedora session without noninteractive sudo, the operator
must review and run the separately documented safe root installation command
interactively. The existing RPM prototype targets RHEL 10, not Fedora; do not
force its distro gate or run the broad day-0 deployment to obtain a preview.

### Stop/restart without deleting data

A failed `podman run` can leave a created container reserving its name. The
launcher reports the original error plus inspection instructions, never
silently replaces it, and never adds `--replace`. Before retrying after a
failure or a name conflict, inspect the existing container:

```sh
podman container inspect honenode-preview
podman logs --tail 80 honenode-preview
```

Only after confirming it is your disposable preview, stop/remove it and retry
using the already loaded image:

```sh
podman stop --time 10 honenode-preview
podman rm honenode-preview
python3 -B packaging/oci/run_local.py
```

Removing the container leaves `.local-run/` intact. The launcher deliberately
does not use `--replace` or delete state. Do not remove state to “fix” an existing
account; preserve backups. Rootless networking, a valid static artifact,
readiness, SELinux broker policy, and integrated provisioning still require
separate runtime validation.

## Real preview smoke test (opt-in)

The interrupted Fedora preview validation was resumed on 2026-11-10; see
[`VALIDATION.md`](VALIDATION.md) for the exact scope and results. Local success
is not a release certification or evidence of live tenant provisioning.

With the existing `honenode-preview` container running, the local Playwright
installation at `backend/.build-tools/browser/node_modules/playwright`, and
Chromium already installed under `backend/.build-tools/browser-cache`, run
from the repository root:

```sh
PLAYWRIGHT_BROWSERS_PATH=backend/.build-tools/browser-cache \
  node packaging/tests/preview_smoke.cjs
```

This is a **real runtime test**, separate from the mocked Python suite. It
requires local rootless Podman and enforcing SELinux. It reads the existing
private bootstrap without printing credentials, checks confinement, HTTP
health and security headers, signs in, renders the neutral operator pages,
checks the packaged CSS, and sends 100 health requests in batches of 10.
It creates no tenants or services and mounts no broker.

To additionally restart **only `honenode-preview`** and verify that the same
browser session, database file and bootstrap survive:

```sh
PLAYWRIGHT_BROWSERS_PATH=backend/.build-tools/browser-cache \
  node packaging/tests/preview_smoke.cjs --restart
```

The restart is explicit and briefly interrupts the preview. Neither form
creates/replaces a container, installs dependencies, changes credentials, or
resets state. Browser call logs are withheld on failure because they can
include filled credentials; the failing stage is reported instead.

`packaging/tests/browser_smoke.cjs` remains the separate, isolated native-app
workflow test. It uses a simulated Unix-socket broker for tenant creation,
catalogue provisioning, stop/start, quotas, authorization and responsive UI;
its screenshots and successes are not real host-service evidence.

## Offline tests

```sh
python3 -B -m unittest discover -s packaging/tests -v
```

Tests use temporary fixture files only. `readelf`, Buildah and Podman subprocesses
are mocked; no host configuration, service, package, image store or running
container is changed. Archive tests check real tar/JSON bytes, every referenced
blob's digest and length, layer contents/ownership, architecture, deterministic
output, unsafe input rejection, and output failure cleanup. Launcher tests check
rootless/local gating, optional load, all confinement arguments, private/reused
bootstrap with the app's account checksum, error propagation and explicit-only
broker access. These are **not** claims of successful static execution, Podman
load, HTTP sign-in, host installation, or live provisioning.

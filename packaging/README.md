# HoneNode packaging prototype (experimental)

**The 0.8.8 evaluation build is not a production release or a working
whole-product installer.** The RPM path packages **only the prebuilt `honed`
host agent**; it does not install or run `honenode`. The OCI path builds a
separate app image and does not make the host deployment ready. For the
OS-tagged app+broker bundles, see [`os/README.md`](os/README.md); the historical
Rocky evaluation bundle remains in [`release/README.md`](release/README.md).
The Fedora native-host deployment scripts, the lab wizard and their evidence
live in the SharpNode host repository, outside this app source tree.

The Fedora and Rocky evaluation bundles are now built separately and tagged in
`manifest.json`; see [`os/README.md`](os/README.md). The backend is shared, but
a binary built on one OS is not assumed to boot on the other.

```sh
bash packaging/os/fedora/build.sh   # on Fedora
bash packaging/os/rocky/build.sh    # boots Rocky 10 in a container
```

No compilation, host installation, service lifecycle operation, or network
access is performed by the packaging tests. No Docker daemon, Compose file, or
container-definition file is required.

## Layout

- `rpm/honed.spec` — RHEL 10-gated, prebuilt-artifact, host-agent-only RPM.
- `rpm/honed.service` — root broker with an empty tenant allowlist and no
  automatic restart.
- `rpm/honenode.sysusers.conf` — the `honenode` group used for socket access.
- `rpm/honenode.tmpfiles.conf` — `/etc/honenode`, `/run/honenode`, and
  `/var/lib/honenode` ownership and modes.
- `oci/build_archive.py` — direct stdlib OCI archive assembly from a prebuilt
  static app and the complete static tree; no Docker or Buildah required.
- `oci/run_local.py` — detached rootless Podman app preview with private bootstrap
  and persistent `packaging/.local-run` state; no broker needed for server preview.
- `oci/README.md` — Fedora preview commands, confinement, bootstrap and limitations.
- `oci/build_image.py` — optional legacy Buildah-from-`scratch` image assembly.
- `tests/` — offline Python tests with mocked readelf/Buildah/Podman subprocesses.

## RPM: experimental host agent only

### RHEL 10 build gate

Build the binary and RPM on **RHEL 10**, for the same architecture and runtime
ABI as the target. The spec deliberately raises an RPM parse error when the
`rhel` macro is not `10`; this prototype has no compatibility claim for other
distributions or releases. `readelf` checks that the input is ELF, but that is
not an ABI or deployment-readiness test.

Required build tools are `rpmbuild`, `systemd-rpm-macros`, and `binutils`. The
spec does not invoke Cargo or download anything. Supply an already built
`honed`; do not treat the workspace version as evidence of a product release.
The `0.1.experimental` RPM release marks its pre-release status.

From this directory, stage the sources without changing the source tree:

```sh
TOPDIR="$PWD/rpmbuild"
mkdir -p "$TOPDIR/SOURCES"
install -m 0755 /absolute/path/to/honed "$TOPDIR/SOURCES/honed"
install -m 0644 \
  rpm/honed.service \
  rpm/honenode.sysusers.conf \
  rpm/honenode.tmpfiles.conf \
  rpm/README.honed \
  rpm/LICENSE \
  "$TOPDIR/SOURCES/"
rpmbuild --define "_topdir $TOPDIR" -ba rpm/honed.spec
```

The resulting RPM is under `rpmbuild/RPMS/`. Installing it creates the group
and directories and asks systemd to reload unit definitions. It does **not**
enable, start, or restart `honed`. Upgrade does not restart a running broker.
Uninstall does not stop it, remove the group, remove an operator policy, or
recursively delete state. RPM may remove only package-owned files and empty
package-owned directories according to normal RPM semantics.

A single-command install is desirable eventually, but is deliberately **not**
provided now: it could falsely imply that SELinux, operator bootstrap, tenant
policy/capacity, and listener/edge readiness had been completed.

### Broker defaults and fail-closed overrides

The unit was checked against `backend/crates/honed/src/config.rs` and the
version-1 schema in `backend/crates/honed/src/policy.rs` and
`backend/crates/honed/README.md`.

| Setting | Upstream default | Packaged behavior |
|---|---|---|
| `HONE_BROKER_SOCKET` | `/run/sharpnode/broker.sock` | `/run/honenode/broker.sock` |
| `HONE_POLICY_FILE` | `/etc/honenode/policy.json` | Same path, explicitly set; file absent |
| `HONE_TENANTS` | `t01,t02,t03,t04` | Explicitly empty |
| `HONE_QUADLET_USERS` | `/etc/containers/systemd/users` | Preserved explicitly |
| `HONE_TENANT_DATA` | `/srv/hosting` | Preserved explicitly; not provisioned |
| `HONE_ETC` | `/etc/hosting` | Preserved explicitly; not provisioned |
| `HONE_CADDY_DIR` | `/etc/hosting/caddy` | Preserved explicitly; not provisioned |
| `HONE_CADDY_ADMIN` | `127.0.0.1:2019` | Preserved explicitly; no Caddy configured |
| `HONE_TENANT_DOMAIN` | `sharpnode-hosting.lab` | Preserved explicitly; must be reviewed |
| `HONE_SUBID_COUNT` | `65536` | Preserved explicitly |
| `HONE_HOST_MEMORY_TOTAL` | detected `MemTotal` | Unset; operator capacity review required |
| `HONE_HOST_MEMORY_RESERVE` | 25% of host total | Unset; operator capacity review required |
| `HONE_TENANT_MEMORY_MAX` | 50% of admission budget | Unset; operator capacity review required |
| `HONE_IMAGE_NGINX`, `HONE_IMAGE_POSTGRES`, `HONE_IMAGE_FILEBROWSER`, `HONE_IMAGE_FORGEJO` | Existing remote image references | Unchanged; the RPM pulls no images |

`/etc/honenode/honed.env` is an optional, operator-created override file read
after these defaults. It is not shipped. In particular, an operator must not
add tenants until their accounts, Quadlet layout, data paths, subordinate IDs,
and policy entitlements have been reviewed.

The broker runs as root with supplementary group `honenode`. It creates its
socket as mode `0660`; `/run/honenode` is `root:honenode` mode `0750`. Add only
the intended app/platform identity to that group after integration review.

### No enabled example policy

The RPM installs **no** `policy.json`, no renamed example policy, and no
preprovisioned tenant IDs. Missing policy already makes provisioning and
activation fail closed; `ping`/`status` do not prove provisioning readiness.

An operator-reviewed version-1 policy must be a root-owned, non-group/other-
writable regular file with trusted ancestors. Its required top-level fields are
`version`, `host_cpu_quota_percent`, `max_services_per_tenant`, `plans`, and
`tenants`. Every plan requires `code`, `allowed_kinds`, `cpu_weight`,
`memory_high`, `memory_max`, `tasks_max`, `cpu_quota_percent`, and
`max_services`. Every tenant entry requires `tenant_id` and `allowed_plans`.
Unknown fields, invalid references, duplicate entries, excessive CPU budgets,
and invalid limits are rejected. Both `HONE_TENANTS` and the policy must permit
a tenant. Consult the broker README for the complete trust and admission rules;
do not copy its example into production without host-specific review.

## Fedora local app preview: direct OCI archive, no Buildah CLI

Use [`oci/README.md`](oci/README.md) for the complete run snippet. The direct
`oci/build_archive.py --binary ... --static-dir backend/static --output ...`
path reuses the static ELF gate and writes an OCI archive that `podman load`
can consume without Docker, Buildah, compilation, or a base-image pull.
`oci/run_local.py --archive ...` loads it and requests a detached, loopback-only
rootless container, with restrictive resource/security flags, private generated
bootstrap credentials, and persistent `packaging/.local-run/data` storage.

This is an independent app/server preview, **not live provisioning**. It does
not require a host broker. `/run/honenode` is private/empty unless the operator
explicitly supplies `--broker-dir`; `t91,t92` are reserved lab IDs only. It never
installs host services, modifies host `/etc`, or invokes sudo. Existing RPM
packaging is unchanged and remains RHEL 10-only.

## Optional legacy OCI PoC image: Buildah from scratch

Prerequisites are Python 3, Buildah, and `readelf`. No Docker tooling is used.
The builder performs no compilation and pulls no base image:

```sh
python3 packaging/oci/build_image.py \
  --binary /absolute/path/to/honenode \
  --css backend/static/hone.css \
  --image localhost/honenode-app:poc
```

Run that command from the repository root, or adjust the CSS path. The script:

1. requires regular, non-symlink inputs and nonempty CSS;
2. uses `readelf -hW`, `-lW`, and `-dW` to require ELF with **no `INTERP`
   program header and no `DT_NEEDED` entries**;
3. runs `buildah from scratch`, copies only the app binary, CSS, and empty
   runtime/state directories, and configures numeric non-root UID/GID `65532`;
4. sets `HONE_STATIC_DIR=/app/static`, the HoneNode registry/socket paths, and
   an empty `HONE_TENANTS`; no operator password or API token is baked in;
5. commits in OCI format and removes the working container on success or
   failure. A cleanup failure after an otherwise successful build is fatal.

`scratch` contains no distribution, shell, or package manager, and the script
adds none. The image exposes port `8080` only as metadata; it does not publish
a listener or configure TLS, Caddy, a firewall, or a broker connection.

### Static does not mean “musl”, and no Rust target is claimed

The `readelf` gate checks static linkage evidence only. A static **musl** binary
and a static **glibc** binary are different artifacts: static glibc can retain
NSS, locale, `dlopen`, and resolver caveats, while musl has its own compatibility
behavior. This prototype does not identify the libc from `readelf`, does not
claim either one, and does **not** claim that any Rust target has been built or
verified. Supply and test the actual prebuilt binary separately. Host `honed`
is not required to be static.

## Offline validation

From this directory:

```sh
python3 -B -m unittest discover -s tests -v
systemd-analyze verify rpm/honed.service
```

The Python suite tests static-ELF acceptance/rejection, direct OCI tar/JSON
structure and blob digests, recursive safe static assets, archive failure cleanup,
rootless launch confinement, private/reused bootstrap and account validation,
explicit-only broker mounting, plus the legacy Buildah workflow. All container
and readelf subprocesses are mocked; tests do not modify host configuration or
start containers, and are not an OCI runtime smoke test. `systemd-analyze verify` can additionally complain when
`/usr/bin/honed` is absent on a non-target host; that is not a successful RHEL
package validation.

A real release gate still needs an RHEL 10 `rpmbuild`, RPM installation in a
disposable VM, `systemd-analyze verify` against the installed binary, Buildah
or direct-archive Podman load/inspection, static-binary execution, and explicit broker/app integration
tests. Those steps must not be inferred from the offline tests.

## Remaining release blockers

- **SELinux:** no confined `honed` domain or app-to-broker socket policy is
  shipped. Do not disable SELinux or use `label=disable` as a release answer.
- **Operator bootstrap:** the local preview stores a secure random initial
  password and valid account number in a private environment file. Production
  secret delivery, rotation and recovery are not implemented.
- **Listener/edge readiness:** the local launcher publishes loopback HTTP only
  with non-secure cookies for preview. TLS/Caddy, firewall, production cookie
  behavior and health checks still need review. Building an archive alone does
  not expose or validate a listener.
- **Policy and capacity:** no active policy, tenant entitlement, CPU/memory
  sizing, or legacy-unit migration is supplied.
- **Host integration:** tenant accounts, subordinate IDs, linger, Quadlet,
  rootless user managers, Caddy, image availability, and socket group membership
  are not orchestrated or verified.
- **Artifact assurance:** RHEL 10 package/image builds, signatures, provenance,
  vulnerability/SBOM review, upgrade/uninstall tests, and runtime smoke tests
  remain outstanding.

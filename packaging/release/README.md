# HoneNode 0.8.0 experimental Rocky x86_64 evaluation bundle

See [`EVALUATION.md`](EVALUATION.md) for the readiness decision, measured
evidence, Rocky/SELinux boundaries and remaining release gates.

This directory builds a **reproducible, experimental evaluation bundle**, not a
production release or a Rocky Linux compatibility/security certification. It is
not a one-command install. The builder assembles prebuilt artifacts only: it does
not compile code, pull images, load a container, install files, start services,
or provision tenants.

## Build

Run from the repository root with Python 3 and `readelf` (normally from
binutils). No Docker daemon, Buildah, Podman, network access, or container runtime
is needed to build. The current static artifact paths are:

```sh
python3 packaging/release/build_release.py \
  --app-binary backend/target/x86_64-unknown-linux-musl/release/honenode \
  --broker-binary backend/target/x86_64-unknown-linux-musl/release/honed \
  --static-dir backend/static \
  --output packaging/release/honenode-0.8.0-x86_64.tar.gz \
  --version 0.8.0
```

The output parent must already exist. Both inputs must be regular, non-symlink,
static ELF files with no program interpreter or `DT_NEEDED` entries. They must
have the same architecture, and this target accepts only x86-64 (`amd64` in OCI
metadata). These checks do not identify musl versus glibc, execute either binary,
or prove Rocky kernel/userspace compatibility. Static assets and every packaged
source must also be regular files/directories without symlinks or special files.

The builder snapshots its inputs in a temporary workspace and reuses
`packaging/oci/build_archive.py` for the app-only OCI archive. The final tar has
sorted paths, fixed ownership/timestamps, normalized modes, and no links. Gzip
uses mtime zero and no staging filename. Identical inputs therefore produce
identical bytes. Output is replaced atomically only after the complete archive
has been written; a failure preserves an existing output.

## Bundle layout and integrity

The archive contains one `honenode-0.8.0-x86_64/` directory:

| Path | Purpose |
|---|---|
| `oci/honenode-app.oci.tar` | App-only OCI image archive, tagged `localhost/honenode-app:0.8.0` |
| `native/honenode` | Static native app fallback |
| `native/static/` | Static assets for the native fallback |
| `broker/honed` | Static host broker binary; not a tenant workload runtime |
| `examples/systemd/` | Source `honed.service`, sysusers, and tmpfiles examples |
| `examples/policy.example.json` | Source policy **example only**, never an active policy |
| `examples/app.env.example` | Generic app environment example with an empty tenant allowlist |
| `examples/quadlet/honenode.container` | Optional app-only, loopback-published Quadlet example |
| `LICENSE` | Repository license notice |
| `manifest.json` | Version, x86_64/OCI architecture, sizes, and SHA-256 for each payload artifact |
| `SHA256SUMS` | SHA-256 for every payload artifact and `manifest.json` |

`SHA256SUMS` deliberately does not list itself, and the manifest does not hash
itself. Paths in both files are relative to the versioned bundle root. After
extracting on the evaluation host:

```sh
cd honenode-0.8.0-x86_64
sha256sum -c SHA256SUMS
```

These hashes detect accidental changes; they are not a signature, provenance
attestation, SBOM, vulnerability assessment, or trust anchor. No credentials,
operator bootstrap secrets, API tokens, active tenant policy, runtime database,
tenant data, or prior local-run state are included.

## OCI app and native fallback boundaries

**The OCI app alone cannot provision tenants or workloads.** It contains the
`honenode` application and static assets, not `honed`, host accounts, rootless
tenant runtimes, a broker socket, host policy, or edge configuration. Loading the
archive is optional and is not part of the build:

```sh
podman load --input oci/honenode-app.oci.tar
```

That command only imports an image; it does not start or validate it. The OCI
configuration uses numeric UID/GID 65532, port 8080 as metadata, secure cookies,
and an empty `HONE_TENANTS`. No operator credential is baked in. Publishing a
port, persistent storage, operator bootstrap, TLS, and any broker mount require
separate review.

The Quadlet file is an **uninstalled app-only example**. It publishes only to
host loopback, disables secure cookies solely for that HTTP evaluation, uses a
named runtime volume, and deliberately mounts no broker socket. Consequently it
cannot provision. It does not enable linger, configure a user manager, or make
the deployment SELinux-ready.

For the native fallback, review `examples/app.env.example` and invoke
`native/honenode` with the intended environment and `native/static` path. The
example is not loaded automatically. Its insecure-cookie setting is only for
loopback HTTP evaluation; do not carry it to an exposed listener. Its registry,
tenant-data, and broker-socket paths are examples, not directories or sockets
created by this bundle. Supply operator credentials through a separately
reviewed private mechanism, never by editing secrets into the bundle.

## Rocky host prerequisites and broker boundaries

Use a disposable Rocky Linux x86_64 evaluation host and verify the actual Rocky
release, kernel, systemd, and runtime versions yourself. No minor-release or
production compatibility claim is made. Depending on the chosen evaluation
path, prerequisites include:

- Podman to load/run the OCI app, unless using the native app fallback;
- systemd for the host broker and any Quadlet evaluation;
- for tenant workloads, working rootless Podman and Quadlet, per-user systemd
  managers and linger, cgroup v2 delegation, subordinate UID/GID allocation and
  helpers, suitable networking/storage drivers, and local NSS/account tools;
- reviewed tenant images/registry access, capacity limits, persistent data
  paths, and an explicitly designed TLS/listener/edge configuration.

The **host broker needs root/systemd**, all applicable rootless tenant
dependencies, an operator-reviewed active policy, deliberate socket-group
membership, and a reviewed SELinux policy. Copying or extracting this bundle
does not satisfy any of those requirements.

The files under `examples/systemd/` are source examples, not installed units or
applied sysusers/tmpfiles configuration. The unit runs `honed` as root with an
empty `HONE_TENANTS` allowlist and expects `/etc/honenode/policy.json`. The
shipped `policy.example.json` contains illustrative plans and tenant IDs; it is
**not active policy** and must not be renamed or enabled without host-specific
review. Missing policy and empty entitlements are fail-closed conditions, not a
broken one-command setup.

The sysusers example defines the `honenode` group. The broker socket is intended
to be mode `0660` in a root-owned, group-restricted runtime directory. Add only
the intended app/platform identity to that group after reviewing the trust
boundary. Group membership alone does not grant SELinux access or configure
rootless tenant dependencies.

## SELinux, SIEM, and edge limitations

**This bundle is not SELinux-ready and ships no confined broker domain or
app-to-broker socket policy.** Keep SELinux enforcing. Review and test policy for
the broker, rootless tenant workloads, sockets, and any mounted volumes. Do not
use disabling SELinux or container labeling as a substitute for that review.

Remote SIEM integration is optional and external: use a log shipper to forward
application/journal logs to the SIEM over TLS. A remote SIEM is **not an app
dependency**, and this bundle neither installs nor configures a shipper, SIEM
credentials, or TLS for it.

A WAF belongs **inline at the edge**, in the reviewed request path before the
app, rather than being treated as an application dependency. No WAF, reverse
proxy, TLS certificate, firewall rule, health check, or production cookie policy
is supplied or validated here.

## Remaining evaluation gates

Before any production consideration, perform separate runtime smoke tests,
broker/app integration tests, tenant isolation and resource-limit tests,
operator bootstrap/rotation review, image and dependency vulnerability review,
SELinux denials review, backup/recovery and upgrade testing, and artifact
signature/provenance work. Successful offline packaging tests or `readelf`
checks are not evidence that those gates have passed.

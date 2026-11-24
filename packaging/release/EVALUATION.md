# HoneNode 0.8.0 — readiness evaluation

## Decision

**Approved as an experimental Rocky x86_64 evaluation artifact. Not approved as
a production-ready or Rocky/SELinux-certified release.**

The meaningful boundary is:

- **Demonstrated:** the native app + host broker can provision real services,
  and the generic panel can pull and start an approved registry image with
  tenant-supplied environment configuration.
- **Packaged:** the app-only OCI, native app fallback, host broker, examples and
  integrity metadata are assembled reproducibly.
- **Not yet demonstrated:** a fresh Rocky host with its own accounts, policy,
  SELinux denials, remote security stack, backup/restore, security scanners and
  upgrade path.

## Evidence on the Fedora workstation

| Area | Result |
|---|---|
| Native web/database provisioning through real tenant UI | Passed previously; real broker and workloads |
| Generic Docker Hub image through tenant UI | `t91-whoami-one` pulled and started; HTTP 200 at loopback port 30001 |
| Generic environment configuration | `WHOAMI_PORT_NUMBER=8080` and `WHOAMI_NAME=sharpnode-registry-check` observed in HTTP response |
| Generic isolation settings | UID 1000, read-only root, no host mounts, broker-selected port |
| Forgejo/Codeberg | Pull refused by upstream HTTP 403 “Codeberg is under load”; not an application-startup result |
| Rust workspace tests | 98 broker + 60 app passed |
| Python packaging tests | 50 passed, including 10 release-bundle tests |
| Fedora harness tests | 68 passed |
| App SQL/theme tests | 33 passed |
| Static musl release build | Passed for `honenode` and `honed`; existing dead-code warnings remain |
| OCI archive | Loaded as `localhost/honenode-app:0.8.0`, amd64, UID/GID 65532 |
| Isolated OCI runtime smoke | Rootless, read-only, resource-limited app started; `/healthz` returned `ok`; no broker mounted |
| Bundle reproducibility | Two independent builds produced identical bytes |
| Bundle integrity | Extracted `SHA256SUMS` passed for every listed payload |

The generic acceptance harness used for that run was part of the SharpNode host
lab, outside this repository. The failed Forgejo reservation was not deleted or
rewritten during that test.

## Artifact

```text
packaging/release/honenode-0.8.0-x86_64.tar.gz
```

The outer archive SHA-256 is reported with the generated artifact. Verify it
with `sha256sum` after copying; the bundle's internal `SHA256SUMS` covers the
payload, not the outer gzip file.

The bundle contains no operator credentials, tenant database, active policy,
tenant data, local-run state, or SIEM secrets. `manifest.json` and `SHA256SUMS`
cover the payload. They are integrity checks, not signatures or provenance
attestations.

## Rocky deployment answer

### Can it be moved to Rocky?

**Yes for an operator-led evaluation.** Use a disposable x86_64 Rocky host,
verify the actual kernel/systemd/Podman/Quadlet versions, install the host
prerequisites, review the example unit and create an explicit root policy. The
OCI app alone is not the whole product.

The host broker `honed` must remain a root/systemd service. It needs:

- a deliberate `HONE_TENANTS` allowlist and active version-1 policy;
- tenant Unix accounts, subordinate UID/GID ranges, linger and user managers;
- rootless Podman/Quadlet, cgroup v2 delegation, networking and storage;
- root-owned policy/configuration and restricted socket group membership;
- operator bootstrap and persistent registry/data paths;
- an SELinux review covering the broker, app socket and tenant volumes.

**Host credentials and SELinux can absolutely cause friction.** The bundle does
not bypass them and ships no confined broker domain or app-to-broker socket
policy. Keep SELinux enforcing; do not use `label=disable`, permissive domains,
or a privileged container as a shortcut. The Fedora native test is evidence of
functionality, not proof that the same socket and volume accesses are allowed
on Rocky.

### Can it communicate with a security stack on another host?

**Yes, with an explicit integration design, not out of the box.**

- **SIEM:** run Fluent Bit on the hosting host and forward approved logs to
  Splunk HEC over TLS. Restrict source/destination firewall rules, verify the
  certificate, protect the HEC token in a root-only shipper configuration, and
  buffer locally. The app must continue provisioning when the SIEM is down.
- **WAF:** ModSecurity + OWASP CRS belongs inline in the HTTP edge path. A remote
  WAF requires reviewed edge→WAF and WAF→app routes, TLS, health checks and
  trusted-proxy/client-IP handling. It is not a setting supplied by this bundle.
- **CTI:** consume minimized SIEM-derived indicators through a read-only role or
  reviewed export. Do not give CTI, tenants, or browsers SIEM administrator
  credentials or broker access.

See `deploy/security-stack/README.md` for the current **WAF + SIEM only** scope;
there is no IDS/IPS component in that scope.

## Gates still open before calling it production-ready

1. Fresh Rocky installation and native broker/app integration with enforcing
   SELinux, including actual tenant creation and service start/stop/restart.
2. Four manually provisioned demo tenants, cross-tenant denial, resource limits,
   reboot behavior and recovery of failed reservations.
3. Lynis, Trivy, OpenSCAP, `cargo audit`, SAST/DAST and dependency/image review
   on the bare-metal target, with findings triaged by the CTI team.
4. Remote ModSecurity/Splunk deployment, TLS/firewall/token/retention tests and
   proof that SIEM/WAF outages do not block provisioning.
5. Backup/restore, tenant data deletion semantics, credential rotation,
   upgrade/rollback, artifact signatures/SBOM and a supported registry strategy.
6. File Browser live bootstrap test and a decision about its archived upstream;
   it is not included in the demonstrated generic acceptance.

No Lynis, Trivy, OpenSCAP, `cargo audit`, Rocky installation, remote SIEM/WAF,
or production security certification was performed during this packaging step.

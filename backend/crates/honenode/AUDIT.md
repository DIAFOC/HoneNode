# HoneNode application audit — 2026-11-09

## Scope and evidence

Writes were confined to this crate and `backend/static/hone.css`. The stylesheet now supplies neutral defaults and optional SharpNode dark-theme overrides. The broker crate, workspace Cargo manifest, packaging and packaging README were not edited. Relevant context: `SN_4.1.md` §§4, 14 and the final handoff/latest broker discussion in `Sources/Food/Spit/SharpNode Rocky setup and HoneNode refactor.md` (approximately lines 97633–97999). Broker findings below are integration dependencies, not certification of another agent's implementation.

This is **not a production approval**. At initial inspection no cargo/rustc/rustfmt was found in PATH or the usual local toolchain locations. The parent owns Rust compilation and reports the preceding workspace revision passed 43 broker + 19 app tests, including Unix-socket tests unsandboxed. The current correlation/PSI changes still need parent validation; this agent has not run Cargo concurrently. This agent performed no host installation, container execution, remote VM changes or toolchain download.

## Concrete fixes

- Require the existing API bearer token for status and broker probes, as already required for tenant/audit APIs. `/healthz` remains public and returns no deployment data.
- Add database-backed, expiring, one-use login challenges bound to an HttpOnly cookie and a hidden form field. Challenge storage is capped at 4096 live challenges.
- Rate-limit valid-format login attempts: 10/account/15-minute fixed window and 60 globally/minute. Two concurrent password-verification tasks maximum, with permits retained inside blocking tasks even if an HTTP request is cancelled. This is application containment, not edge DoS protection.
- Run password KDFs off the async executor. Unknown/disabled accounts still perform verification work. New passwords must be 12–1024 bytes; supplied account numbers must have a valid checksum.
- Password replacement and session revocation are one transaction. Do not serialize password hashes with account responses.
- Secure cookies default on. Secure login/session cookies use the `__Host-` prefix, Secure, HttpOnly, no Domain and Path=/. Ignore client-supplied forwarded protocol for cookie security. Reject ambiguous duplicate or malformed cookies.
- Add `Cache-Control: no-store` to responses, including login/credential pages; redact internal/database/broker errors from HTTP bodies while retaining server-side logging. Rate-limit errors include Retry-After.
- Create registry files with mode 0600, restrict existing registry file permissions and refuse symlink/special-file registry targets. Newly created parent directories use mode 0700. Existing parent directories still must be trusted and provisioned securely.
- Validate numeric configuration instead of casting negative/zero values into durations or panicking in interval creation.
- Require tenant role as well as tenant linkage. Tenant controls reject suspended/purged tenants, suspended/pending/failed/deleted services, another tenant's service, and expiry beyond grace. The UI hides controls when blocked.
- Serialize UI broker mutations, billing extensions, status readback and the expiry sweeper within this process. Command refusals preserve prior lifecycle intent, invalidate cached observations and return an error. An acknowledged activation remains pending until actual active readback; failed readback is unknown, not fabricated running.
- Reserve services with one atomic INSERT/SELECT, rejecting inactive tenants, zero quota and an existing reservation. **Fail closed at one historical reservation per tenant**, including deleted rows, because current port/volume ownership is tenant-scoped and there is no verified reallocation/cleanup protocol. This deliberately prevents duplicate units, port collisions, shared data and concurrent quota bypass. It is not a full multi-service quota implementation.
- Ensure the broker tenant exists before provisioning limits/start; retain the existing wire contract. Bound broker answers to 1 MiB, require newline framing and a matching operation name.
- Make concurrent time extensions additive with a SQLite writer transaction; reject extensions on deleted services. Do not purge renewed suspended services while their expiry is newer than the retention cutoff.
- Persist typed broker UID, exact unit, actual systemd state and observation timestamp after commands and periodic read-only status polling. Never derive observed state from command acknowledgement. Separate lifecycle intent from timestamped observations in the UI. Polling never starts services or clears suspension. See `OBSERVATIONS.md`. Separate operator action forms from billing `days` input. Remove frontend claims that disk quotas, MIME inspection, secret scanning and a database console are implemented.
- Flatten typed broker operations with bounded app-asserted actor/request metadata. UI jobs and audit events share the same request ID with command/readback sequences; background calls identify the system actor. Reject missing/mismatched echoed IDs. This is correlation, not independent authentication, authorization or idempotency. `edge.reload` now carries tenant/service identity.
- Use existing pressure telemetry on the operator dashboard: process-visible CPU/memory/I/O PSI, with explicit unavailable states. No load-based automation, extra host permissions, blanket warning suppression or deletion of planned features.
- Default to neutral HoneNode branding and a gray/white/beige theme. `HONE_THEME=sharpnode` opts into the existing dark palette and branding; invalid values fail startup. Shared Askama view fields supply the allowlisted root class and branding boolean to all 13 template construction sites. No JavaScript, inline CSS or CSP changes. See `THEMES.md`.

## Compatibility / deployment impact

1. Migration `0003_login_security.sql` adds login challenge/limiter tables. New migration `0004_service_observations.sql` adds an observation timestamp and clears unverified legacy observation strings, without changing lifecycle intent or UID history. The normal migrator applies them; original migrations 0001/0002 were not modified.
2. Plain-HTTP lab/tunnel deployments must explicitly set `HONE_SECURE_COOKIES=false`. Public deployments must use TLS and keep secure cookies enabled. Secure-cookie names changed, so existing browser sessions require sign-in again.
3. `/api/v1/status` and `/api/v1/broker` now require `Authorization: Bearer ...`. Existing unauthenticated probe scripts need adjustment by their owner; use `/healthz` for public liveness only.
4. A tenant with **any** service history cannot provision another service through this application until broker-backed allocation/cleanup and quota policy are implemented. No old rows/data are automatically removed to bypass this guard. Failed reservations require operator investigation, not blind retry.
5. Invalid duration/boolean configuration and weak bootstrap passwords now fail explicitly. The mutation lock is intentionally coarse and process-local; run one application writer, not multiple independent app replicas.
6. Broker replies must now echo `request_id`; deploy a matching broker. IDs join logs/jobs/audit only and do not prevent duplicate side effects. Job payloads gained a `correlation` object; existing database columns/migrations are unchanged by the correlation update.

## Validation

Executed successfully:

```sh
python3 -B backend/crates/honenode/tests/test_registry_sql.py -v
python3 -B backend/crates/honenode/tests/test_theme.py -v
```

Eleven tests currently pass against the actual migration/query text: eight competing SQLite writers produce exactly one reservation; historical reservations remain reserved; inactive tenants and zero quota are rejected; limiter increments/window reset work; login challenges are one-use/expire; renewed suspended services are not selected for purge; observation updates promote only explicit active pending records, preserve policy and tombstones, enforce identity guards, select observation timestamps, reset legacy unverified reports without changing policy/UID history, and restrict telemetry to valid recent observations of live reservations.

Three stylesheet/markup tests additionally verify neutral text contrast (at least 4.5:1), control-boundary contrast (at least 3:1), explicit dark-color opt-in, absence of inline styles/scripts and CSRF fields on all 13 POST forms.

The parent reports the preceding workspace revision passed 43 broker + 19 app tests. The current app suite has 23 Rust tests across `src/tests.rs`, `src/broker.rs`, `src/observation_tests.rs` and `src/telemetry.rs`, pending parent compilation/execution. Coverage includes auth/CSRF, additive extensions, observation persistence/policy guards, both themes, empty-plan UI, strict correlation/scoped-edge serialization, echoed-ID checks, operator/tenant jobs/audit joins, spoofed-header rejection, and PSI parsing. Exact wire assertions forbid raw app-controlled limits in `limits.set`; a fake-broker test deliberately corrupts display limits while requiring only `Plan.code` to be sent. Three useful previously unused template context fields now render empty states; no dead-code suppression was added and meaningful future functions were retained. Python SQL tests do not validate SQLx time encoding, Axum handler traits, Askama compilation or Rust concurrency semantics.

Coordinate with a toolchain/build owner to run formatting, `cargo test -p honenode` and `cargo clippy -p honenode --all-targets` in an approved build workspace. At initial inspection the checkout had no workspace lockfile; resolving dependencies and creating build artifacts/lockfiles must respect other agents' ownership. Browser, TLS proxy and live Rocky/Podman end-to-end tests are also outstanding.

## Remaining production blockers / integration handoff

### Security and exposure

- **Operator and tenant routes still share one listener/router.** Authentication is not network separation. Do not expose the operator dashboard via the public proxy. The master requires an internal operator surface; separate listeners/routing and deployment evidence are still needed. Operator login itself also needs the intended exposure policy.
- Forwarded source addresses in session/audit context are still untrusted request headers, not authenticated peer/proxy identity. Do not use them for bans or security decisions. Define trusted proxies, strip forwarded headers at ingress and capture transport peer identity. Login limits deliberately do not trust XFF.
- Global/account login limits can be used to deny legitimate logins; challenge GETs can exhaust the bounded pool. Edge rate limits/timeouts/body limits are necessary. No MFA, session inventory/revoke UI, or complete credential recovery/reset/reissue interface exists. Support role has no completed read-only surface.
- Existing deployments must verify registry-parent ownership, TLS, cookie configuration, socket permissions, SELinux policy and distinct registrable domains for portal vs tenant content. Configuration does not prove those properties.

### Provisioning, quotas and broker ownership

- **Tenant self-service provisioning remains absent.** There is no authoritative tenant plan/subscription entitlement gate. Reusing the operator form would allow tenants to choose their own plan and grant arbitrary service time. Define the entitlement policy before exposing provisioning.
- The broker owner is implementing authoritative plan/tenant policy and CPU/memory admission. The app now sends only the selected plan code, but final schema/configuration and end-to-end acceptance remain required. Multi-service allocation, aggregate limits, disk/bandwidth controls and actual enforcement still need acceptance evidence; app-side one-reservation containment does not certify those controls.
- There is no durable outbox/idempotency/reconciliation protocol. A timeout can mean a host mutation succeeded. Jobs and service reservations are separate writes; cancellation/restart can strand queued/running jobs or pending/failed reservations. There is no safe retry/repair UI, compensation or verified cleanup/reallocation. Process-local serialization is not cross-process fencing.
- Tenant account/slug creation is not one database transaction. Process serialization prevents same-instance races but SQL/broker failure or competing instances can leave orphan identities or unavailable slots. Failed tenant host creation still needs recovery.
- Typed UID/unit/state observations and timestamps are now integrated in the app, with fresh-UID sampling. Host cgroup visibility inside the app container, permissions, live broker behavior and health/readiness remain unverified; CPU/disk metering and authoritative port discovery remain incomplete. The app now sends only `plan_code` plus tenant/unit/kind to `limits.set`, matching the current broker policy resolver; the parent must confirm the final contract and validate deployment policy alignment. No raw limits are sent.

### Lifecycle, frontend and product completeness

- Automatic renewal/resume and tenant grace-state recovery are incomplete. Adding time does not automatically restart a suspended service. Suspension reason is not modelled separately from billing expiry; retention/purge policy needs explicit review and tests, including database/git retention periods. Unit deletion is not proof of data erasure. Do not advertise verified data purge or backups/restores.
- Public links are inferred from slug/domain, not validated publication evidence. No complete tenant-bound publish/reload/certificate/probe workflow exists in the UI. Logs, service credentials/rotation and database console UX are incomplete. Payment collection/metered billing are not implemented; operator-granted time must be described as manual administration, not proof of payment.
- File Browser is identified in the master as archived/EOL. Replace it or formally accept a pinned version with compensating controls before shipping storage. Catalogue image references and UI controls are not security acceptance evidence.
- First-run web setup, removal of hardcoded SharpNode paths/default tenant allowlist and complete independent-product workflows remain master-document release gaps. The minimal neutral/default vs SharpNode/optional theme boundary is now implemented; visual/browser acceptance remains outstanding. Packaging, signed/pinned artifacts, upgrades/rollback, SELinux, SBOM, reboot/isolation/restore and fresh-host acceptance evidence are outside this edit scope and were not revalidated.
- The preceding app revision compiled/passed tests per the parent, but the current correlation/PSI changes and warning count still require a fresh parent build. Dependency/security auditing and browser/end-to-end functionality remain **unverified**, not passing by inference. No production-ready claim is justified until those checks and the blockers above are resolved.

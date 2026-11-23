# Broker service observations

The app now uses the broker's read-only `service.status` operation:

```json
{"op":"service.status","tenant_id":"t01","service":"t01-site","request_id":"req-1","actor":{"kind":"system","id":"honenode"}}
```

A successful status answer must contain `ok: true`, the matching operation, **all three** observation fields (`uid`, `unit`, `active_state`), and no non-null error. The client accepts only a nonzero u32 UID other than the invalid UID sentinel, an exact requested unit without `.service`, and these systemd states:

```text
active reloading inactive failed activating deactivating maintenance refreshing
```

Missing/null fields, negative/out-of-range/string UIDs, unknown state strings, contradictory success/error, another tenant/unit, suffixes, paths and malformed unit names are errors. Units are checked against the tenant prefix and the broker's ASCII/length contract. The client does not derive identity from free-text `detail`. Framing remains bounded to 1 MiB with a matching operation and newline. Status readback has a five-second deadline.

## Correlation, not authentication or idempotency

All network calls flatten the typed operation with `request_id` and `actor: {kind, id}`. IDs must be nonempty bounded ASCII letters/digits or `-_.:@` (64 bytes for request IDs; 128 for actor IDs). Actor kinds are typed `operator`, `tenant`, or `system`.

UI mutations derive the actor from the authenticated app principal: its role and account number, matching the app audit actor identifier. They reuse the audit request ID throughout tenant creation/provisioning/lifecycle command sequences **and their status readback**. The job's existing `requested_by` still records the account database ID; its JSON payload stores the wire metadata under `correlation`, so the job, app audit event and broker journal can be joined without a schema migration. Request/actor headers are not trusted for this metadata. A missing entropy-generated audit ID prevents sending a mutation instead of reusing the shared `unavailable` fallback.

Generic/background calls generate an ID and assert a system actor (`honenode`, or `honenode:lifecycle` for lifecycle sequences). The operator dashboard's broker probe uses its actual operator principal. Replies must echo the exact request ID on success and refusal, in addition to matching the operation; missing/mismatched echo is a protocol error. Deploy the matching broker that supports echoing IDs.

**These fields are app-asserted correlation metadata, not independent broker authentication/authorization, proof of human identity, replay protection or idempotency keys.** Several RPCs intentionally share one workflow ID. The client does not automatically retry or deduplicate requests, and an echo does not prove that a host action happened only once. Broker Unix-peer authorization, ownership checks and policy remain separate controls.

`edge.reload` now requires both `tenant_id` and `service` in the app's typed request, in addition to hostname/upstream. The broker must validate the real service/route binding; no public publishing workflow was added here.

## Persistence and lifecycle behavior

Migration `0004_service_observations.sql` adds `services.observed_at`. UID, actual systemd state and collection timestamp are saved together, conditional on the service ID, tenant ID, exact unit and a non-deleted reservation. Legacy observation strings without evidence are reset to unknown by the migration; lifecycle intent and UID history are not rewritten.

After provisioning or an acknowledged lifecycle command, the app reads status. A start/restart/resume acknowledgement records pending intent, **not running**. Only an explicit `active` report may promote pending to running. `activating`, `reloading`, `inactive` and other transitional reports are stored verbatim, without inventing `running`. Failed/maintenance readback after activation fails the action. The systemd `active` state is still not an application health/readiness probe.

Command errors invalidate any cached observation but preserve the previous lifecycle intent. Successful command plus failed readback leaves the acknowledged intent (pending for activation, stopped/suspended for policy actions), with an unknown observation and a failed job/error response. A timeout does not prove that the host action failed: do not blindly replay it. Failed provisioning remains reserved for operator investigation.

Deletion is special: after acknowledgement, the deleted unit cannot supply status. Its observation is cleared, not fabricated as `inactive`. No evidence of data erasure is claimed.

## Periodic observation and telemetry

A periodic observer uses the existing `HONE_SAMPLE_SECS` interval (default 60 seconds). It issues only `service.status` calls and database updates: **no start, stop, provisioning, limits, deletion, retry or compensation commands**. It skips deleted reservations and prefers interactive operations using the existing mutation lock. The lock covers each status read and writeback to prevent an older report overwriting a newer action within this process; one app writer is still required.

An active report never clears a suspended, stopped or failed policy state. Active units contradicting a stop/suspend intent generate a warning for operator review. A broker error clears the observation. Pending records may become running only after explicit `active` evidence; this is metadata reconciliation, not a host mutation.

The sampler selects only a recently verified UID (within two sample intervals), validates its integer range and excludes deleted services. This fixes the app-side NULL-UID sampling gap, **not** the deployment's host-metrics visibility. Container cgroup namespaces/mounts and permissions still need validation; CPU/disk metering remains incomplete. No new host mounts or privileges were added.

The UI distinguishes lifecycle intent from the last timestamped broker observation. Dashboard active counts require a recent explicit active report. Old or missing reports do not become health claims. The operator dashboard also displays process-visible CPU/memory/I/O PSI averages and cumulative stall time, or unavailable when absent/malformed. These readings do not certify host-wide namespace visibility and do not drive automatic resource changes.

## Plan policy coordination

`limits.set` operation parameters are only `op`, `tenant_id`, `service`, `kind` and `plan_code` from the selected database `Plan.code` (not its numeric ID or display description). The shared envelope adds the correlation fields described above. No raw CPU, memory, task or CPU-quota values are sent. This matches the current broker `policy::resolve` implementation and its minimal request test; the parent should confirm the final broker schema before accepting the integration. The payload is locked down by serialization and fake-broker tests, including deliberately invalid app-side display limits.

This requires the matching broker policy release and a configured policy/entitlement file. There is no silent fallback to app-controlled raw limits. Display/billing resource metadata in SQLite is not the authority for enforcement and still needs deployment policy alignment.

The app only calls `limits.set` for a fresh reservation, not an in-place plan update. It does not automatically stop an existing unit to bypass the broker's stop-before-update rule. Safe plan changes need an explicit lifecycle workflow. Start/reload ownership validation remains broker authority.

## Tests / build coordination

The new Rust tests cover strict status parsing, all systemd states, UID persistence, policy/tombstone guards, stale reports, provisioning/readback, read-only polling and failure invalidation. Fake socket tests require the same Unix-socket permission exception as the original framing test; they are not silently skipped on EPERM.

Actual-query SQLite tests cover timestamp projections, active-only promotion, policy preservation and identity/deletion guards. Run:

```sh
python3 -B backend/crates/honenode/tests/test_registry_sql.py -v
python3 -B backend/crates/honenode/tests/test_theme.py -v
```

The parent reported the **preceding** workspace revision passed 43 broker + 19 app tests, including Unix-socket tests unsandboxed. The current correlation/PSI changes bring the app suite to 23 tests and still need parent compilation and execution. New coverage checks metadata bounds/flattening, scoped edge payloads, echoed IDs on success/refusal, operator/tenant correlation shared with jobs/audit, spoofed-header rejection, and PSI completeness/finite values. This agent has not run Cargo while the parent works. No broker, workspace, packaging or host edits were made.

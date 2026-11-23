# honed broker notes

## Broker-local policy (required for provisioning and activation)

`HONE_POLICY_FILE` defaults to `/etc/honenode/policy.json`. There is **no fallback policy**. Missing, malformed, untrusted, or unsupported policy refuses `catalog.list`, `tenant.create`, `limits.set`, and start/restart/resume; status and stop remain available. The manifest and its ancestors must be root-owned, non-group/other-writable, and not symlinks. The file must be regular, UTF-8 JSON, at most 64 KiB; open uses `O_NOFOLLOW`. The broker reloads and validates it for each provision/activation operation.

`policy.example.json` in this crate is an example, not an installed/implicitly enabled policy. Packaging must install an operator-reviewed copy as root (typically `0644` or `0600`). Do not mount the app's writable settings/registry as broker policy. The example assumes at least four available CPUs and reserves only 300% (three core-equivalents) for tenant workloads. Size memory separately using the settings below. No host changes are made by shipping this example. `policy.selfservice.example.json` documents the self-service shape: a `t*` wildcard entitlement plus a two-service 7-day `starter` plan and a four-service `pro` plan.

### Manifest schema, version 1

All fields below are required except the documented optional `service_days` and `allowed_registries`; unknown fields, duplicate plan/tenant entries, invalid references, and invalid limit values are rejected.

- `version`: integer `1`.
- `allowed_registries`: optional list of exact registry authorities (e.g. `codeberg.org`, `docker.io`, `registry.example:5000`), default **empty/deny**. No schemes, credentials, namespace prefixes, or wildcard matching. Generic container plans additionally require `container` in `allowed_kinds`.
- `host_cpu_quota_percent`: positive integer, no greater than 100 × broker-detected available CPU count. `100` means one CPU core. Set below this ceiling to reserve CPU for the platform/OS.
- `max_services_per_tenant`: positive integer, at most 1000; total committed services across all plans for any tenant.
- `plans`: nonempty array of objects:
  - `code`: 1–32 lowercase ASCII letters, digits, or `-`; unique.
  - `allowed_kinds`: nonempty unique subset of `web`, `database`, `storage`, `git`, `container`.
  - `cpu_weight`: integer 1–10000 (relative weight, not capacity).
  - `memory_high`, `memory_max`: strict binary-size strings; positive, high ≤ max.
  - `tasks_max`: integer 1–1000000.
  - `cpu_quota_percent`: positive integer ≤ host CPU budget; emitted as native systemd `CPUQuota=N%`.
  - `max_services`: positive integer ≤ `max_services_per_tenant`; when this plan is selected, maximum committed services **across all of the tenant's plans and kinds**. It is not a per-plan-code allowance.
  - `service_days`: optional integer `1..=366`, default `30`. This is fixed operator policy returned to the portal; a request containing `service_days` is rejected rather than allowing a tenant to select or override it.
- `tenants`: array of objects with `tenant_id` and nonempty unique `allowed_plans` (references to plan codes). Both this entitlement and `HONE_TENANTS` must permit a tenant. An asserted actor cannot grant an entitlement. `tenant_id` is either an exact tenant (`t00` through `t99`, two digits, unique) or the wildcard `t*`, which entitles every otherwise unlisted tenant of that shape — exact entries always take precedence over the wildcard. Three-digit tenant IDs are deliberately rejected so the fixed port allocations cannot overflow or collide.

Provisioning counts stopped/suspended services, replaces the old reservation on same-unit updates, enforces the root-wide service ceiling and the selected plan's `max_services` across **all** committed tenant services (including other plan codes), and enforces one unit per tenant/fixed-template kind across all root-discovered unit names (generic `container` permits multiple distinct units). It sums all committed CPU quota percentages across tenants using checked arithmetic. It also independently enforces memory admission. CPU reservations are persisted in `CPUQuota=...%` and plan/kind metadata in `# HonedPlan=...` / `# HonedKind=...` comments in root-owned Quadlets. Activation revalidates the unit against current plan entitlements/limits, reloads systemd, and checks observed CPU quota and memory against the reservation.

### `limits.set` request

```json
{"op":"limits.set","tenant_id":"t01","service":"t01-site","kind":"web","plan_code":"starter","request_id":"job-123","actor":{"kind":"tenant","id":"account-123"}}
```

`plan_code`, `kind`, `tenant_id`, and `service` are required. The broker resolves CPU/memory/task values from its manifest, not the app. Legacy optional fields `cpu_weight`, `memory_high`, `memory_max`, `tasks_max`, and new optional `cpu_quota_percent` are only assertions: every supplied field must equal the plan (memory sizes compare in bytes). Omitting `plan_code` is rejected even when raw limits would match a plan. Update app callers accordingly.

### `catalog.list` request and response

```json
{"op":"catalog.list","tenant_id":"t01"}
```

The tenant must be present in both `HONE_TENANTS` and the root policy's `tenants` entitlement list. The Unix account does **not** need to exist yet, so the portal can query before `tenant.create`. No host account lookup or provisioning is performed by this query. `tenant.create` likewise checks policy entitlement before changing the host.

Success adds a typed `catalog` array. Each entry has exactly the broker-resolved plan fields `plan_code`, `memory_high`, `memory_max`, `cpu_quota_percent`, `tasks_max`, `max_services`, `service_days`, `allowed_kinds`, `images`, and defaultable `registries`. Memory values use the policy's strict size strings. The **live catalog supports `web`, `database`, `storage`, and opt-in `container` kinds**; `images` maps those same kind names to the configured image strings (`HONE_IMAGE_NGINX`, `HONE_IMAGE_POSTGRES`, `HONE_IMAGE_FILEBROWSER`). Generic plans expose the policy registry list as `registries` and have no fixed `images.container`. Git is filtered out until its credential onboarding is implemented, and plans with no remaining live kind are omitted rather than shown as empty/fake choices. Only plans explicitly entitled to that tenant are returned, in policy order. Example entry:

```json
{"plan_code":"starter","memory_high":"256M","memory_max":"384M","cpu_quota_percent":25,"tasks_max":128,"max_services":1,"service_days":30,"allowed_kinds":["web"],"images":{"web":"docker.io/nginxinc/nginx-unprivileged:1.27-alpine"}}
```

### `options` on `limits.set` and `service.configure`

`limits.set` accepts an optional `options` object; `service.configure` requires it. Unknown fields are rejected. `options.image`, when supplied, must exactly equal the broker-configured image for the requested kind; arbitrary image imports are refused. `options.file_manager` is accepted only for `storage` and has `username` (1–32 ASCII letters/digits/`_ . -`), `password`, and `read_only` (default `false`). Initial storage creation requires a 12–72-byte password without control characters; the broker hashes it with bcrypt in-process and never logs, echoes, or returns it. Once provisioned, the password must be empty and the username must match the stored one: the broker never resets or rotates file-manager credentials. `read_only` changes on an existing unit go through `service.configure` on the stopped service. Passwords are redacted in `Debug`, and malformed requests carrying `options` return redacted parse errors.

### `service.inspect` and `service.configure`

```json
{"op":"service.inspect","tenant_id":"t01","service":"t01-files"}
{"op":"service.configure","tenant_id":"t01","service":"t01-files","options":{"file_manager":{"username":"alice","password":"","read_only":true}}}
```

Both require the tenant-bound, root-owned committed unit (kind/plan markers); inspect works while inactive and never looks at a different unit. `service.configure` supports only storage file-manager settings, derives kind and plan from the committed unit (client `kind`/`plan_code`/raw limits are rejected), requires the service to be inactive with no pending job, and re-runs the same policy/capacity admission as `limits.set`. Success responses for `limits.set`, `service.inspect`, and `service.configure` include a sanitized `configuration` object: `{ "kind": "storage", "image": "...", "username": "alice", "read_only": true }`. `username`/`read_only` are present only for storage; no secrets, hashes, or host paths are returned.

### Correlation on every request

Optional `request_id` is a string of 1–64 bytes; optional `actor` is `{ "kind": "operator" | "tenant" | "system", "id": "..." }`, with a 1–128-byte ID. IDs allow only ASCII letters, digits, `-_.:@`. Controls, whitespace, Unicode, excess length, unknown actor kinds/fields, or wrong JSON types are rejected before dispatch. Valid `request_id` is echoed on successful and refused dispatch responses; parse failures cannot echo invalid correlation metadata. Missing IDs remain omitted for backward compatibility.

Audit events carry the correlation span's `request_id`, `asserted_actor_kind`, `asserted_actor_id`, and `actor_is_app_asserted`; `actor_type=unix_peer` / `actor_id` retain the actual peer UID. **The app actor is asserted, not independently authenticated or authorized by the broker.** Request IDs are correlation only, not idempotency tokens, revision checks, or replay protection.

### Migration and destructive operations

Legacy units lacking CPU/plan/kind reservations block new admission rather than count as zero CPU. Activation of legacy units is refused. Units with stale ports, mounts, environment files, or rootless user mappings are also refused by start/restart/resume and `edge.reload`. To migrate a supported web/database/storage unit, stop it and rerun the same unit's entitled `limits.set`, then start it; this re-renders the current layout without deleting tenant data or rotating secrets. Storage units must match the current safe render exactly. The fixed Git template remains refused until its credential/admin bootstrap is implemented; generic containers use their image default startup and tenant-managed application onboarding. A same-unit replacement excludes its old reservation, but other legacy units still block admission, so bulk legacy migration needs an explicit operator-reviewed procedure (not automated here). Policy edits do not retroactively reconfigure or suspend running workloads, and a root operator must reconcile reservations before lowering host budgets.

`tenant.delete` is disabled unconditionally until a retention and independently authorized confirmation design exists. No JSON flag, manifest field, request ID, or asserted operator enables a one-shot account/data wipe. `service.delete` still requires stop and removes the service unit only; tenant data is retained.

Tenant creation enables linger and synchronously starts the exact `user@UID.service`
before reloading user units; `enable-linger` alone can return before the user bus
is ready. Activation uses `systemctl --user --no-block start/restart` after the
same policy/provenance/limit checks. Success acknowledges a queued job, **not**
workload health. A cold image pull may remain `activating`; poll `service.status`
and check HTTP/PostgreSQL readiness. The app keeps the intent pending until an
explicit active observation. Stop remains synchronous so deletion or capacity
changes cannot race a still-running unit. Command/socket bounds are unchanged.

## Data, secrets, and port provisioning

`tenant.create` creates a no-login Unix account and its base data directory. Each successful `limits.set` then creates that kind's mount targets before rendering the unit. Web creates `site/index.html` only when absent; existing tenant HTML and secrets are never overwritten or rotated. Activation also requires that HTML to remain a tenant-owned regular file. Database uses `pgdata`; the retained future Forgejo layout uses `gitea` and `gitea-config`. `HONE_TENANT_DATA` is root-owned `0755` for traversal; tenant directories and their kind-specific children are owned by the tenant UID/GID so the Quadlet `UserNS=keep-id:uid=N,gid=N` mapping gives the container process the intended host ownership. New accounts use `/sbin/nologin`; no shell or web console is provided.

Database environment files are generated from operating-system randomness at `$HONE_ETC/secrets/tNN-db.env`. The Forgejo secret renderer is retained for future onboarding at `$HONE_ETC/secrets/tNN-forgejo.env`, but Git is refused before setup until secure admin creation exists. Secret files are `root:<tenant-primary-group> 0640`; `$HONE_ETC` is root-owned `0755` and its `secrets` child is root-owned `0711`, allowing traversal without directory listing. Existing files must retain exactly that ownership/type/mode. `HONE_ETC` remains overrideable for the reference deployment; unit rendering and provisioning use the same configured path. Repeated tenant/service setup is idempotent: existing secrets are validated, never reset or rotated, and are retained by `service.delete`; secrets are never returned in broker responses.

Storage provisions `vault` and `db` plus a root-owned `$HONE_ETC/secrets/tNN-filebrowser.json` (`root:<tenant-group> 0640`, create-exclusive, never overwritten) containing the username and bcrypt hash for FileBrowser's first-run quick setup. The unit overrides the image entrypoint to `/bin/filebrowser` and passes fixed flags (`--config /config/settings.json --database /database/filebrowser.db --root /srv --port 8080 --address 0.0.0.0 --disableExec=true --followExternalSymlinks=false --noauth=false`), so the admin UI cannot re-enable the command runner, external symlinks, or no-auth mode. The vault is mounted `ro,Z` when `read_only` is true; the database is always `Z`; the settings file is mounted read-only at `/config/settings.json` and never under `/srv`. `DropCapability=all`, `ReadOnly=true`, and a `/tmp` tmpfs are set. A root-owned `tNN-filebrowser.initialized` marker records that the database was observed; if it later disappears, activation and configuration fail closed instead of re-bootstrapping credentials. Fixed-template Git provisioning and activation still return `not_implemented`: the renderer's `INSTALL_LOCK=true` and generated secret/internal token do not create an administrator, so exposing it would produce an inaccessible or unsafe onboarding flow. The fixed `git` kind does not appear in the live catalog; a generic Forgejo image is independent of that template. A successful `limits.set` means rendered and validated, not application-ready; use `service.status` and application health checks rather than treating start as readiness.

Ports are deterministic and cannot be selected by the tenant:

| Kind | Host binding |
|---|---|
| web HTTP | `127.0.0.1:18000+NN` |
| database PostgreSQL | `127.0.0.1:15430+NN` |
| storage HTTP | `127.0.0.1:19000+NN` |
| git HTTP | `127.0.0.1:20000+NN` |
| git SSH | `127.0.0.1:22000+NN` (loopback only while Git is unsupported) |

`edge.reload` derives the required upstream from the unit's committed kind, not from a generic web port.

## Memory admission

`limits.set` validates the complete service envelope and admits its `MemoryMax` reservation before writing a Quadlet unit. Reservations are rebuilt by discovering root-trusted `.container` files across numeric UID directories beneath `HONE_QUADLET_USERS`, independently of `HONE_TENANTS`, the in-memory tenant registry, and NSS. Removing a tenant from the authorization allowlist or losing its Unix account resolution does not release its reservations. New files record `# HonedTenant=tNN`; older broker plan/kind markers and the reserved legacy `tNN-*` filenames also identify managed records. Malformed managed identities/reservations fail closed. Unrelated regular platform units without broker markers or a legacy tenant filename are ignored. Untrusted/symlink entries fail closed rather than being followed. Staging is not traversed. Stopped and suspended units still consume their committed reservation. Updating a unit replaces its previous reservation instead of double-counting it.

Every limit write requires the requested systemd service to be `inactive` with no pending job (or absent for first creation). Active, failed, and transitional states are refused before replacing the Quadlet file. Use `service.stop` → `limits.set` → `service.start`; the broker does not silently restart workloads. This prevents releasing a larger reservation while its workload is still running with the old cgroup limit.

Start, restart, and resume require a root-owned, non-group/other-writable regular Quadlet file under the tenant's UID directory, with a matching `ContainerName` and valid reservation. Symlinks and writable/untrusted ancestors are rejected. Activation reloads the user manager and verifies the reported `SourcePath`, generator `FragmentPath`, and `MemoryMax` before invoking systemd. This also prevents a failed earlier reload from activating a stale larger definition.

Two independent checks must pass:

- the tenant's committed memory must not exceed its tenant limit;
- all tenants' committed memory must not exceed the host budget (`host total - host reserve`).

Malformed existing units, duplicate `MemoryMax` directives, non-regular unit files, and arithmetic overflow fail closed. Concurrent broker operations are serialized so two requests cannot pass admission using the same remaining capacity.

Configuration uses strict binary sizes: bytes, `K`/`KB`, `M`/`MB`, `G`/`GB`, or `T`/`TB`.

| Variable | Default |
|---|---|
| `HONE_HOST_MEMORY_TOTAL` | `MemTotal` from `/proc/meminfo`; a configured value may lower, but not exceed, detected memory |
| `HONE_HOST_MEMORY_RESERVE` | 25% of host total |
| `HONE_TENANT_MEMORY_MAX` | 50% of the host admission budget |

Example for a host whose detected RAM is 16 GiB:

```sh
HONE_HOST_MEMORY_RESERVE=4G
HONE_TENANT_MEMORY_MAX=3G
```

The host admission budget is 12 GiB and each tenant may reserve at most 3 GiB. Set these values explicitly during installation rather than relying on the generic defaults for capacity planning.

## Tenant-bound `edge.reload` wire contract

```json
{"op":"edge.reload","tenant_id":"t01","service":"t01-site","hostname":"t01.tenants.example","upstream":"127.0.0.1:18001"}
```

This is now a tenant operation, **not** a platform operation. Both `tenant_id` and `service` are mandatory; the hostname is never used as a substitute for the authenticated app request's tenant scope. The requested service must belong to that tenant, have a root-owned Quadlet matching current broker policy, and be loaded from its expected generated systemd source. Only web/storage/git kinds may publish HTTP vhosts; database kinds are refused. Git currently fails the unsupported-service validation before this operation. The hostname must exactly equal `<tenant_id>.<HONE_TENANT_DOMAIN>`, and upstream must be that same tenant's assigned loopback port for the committed kind: `18000+NN` for web, `19000+NN` for storage, or `20000+NN` for git. Existing callers must add the two identity fields. App-user identity is still asserted through the shared app connection, not independently authenticated by the broker.

## `service.status` wire contract

One JSON line per connection, using the same tenant allowlist and service-name ownership checks as lifecycle operations:

```json
{"op":"service.status","tenant_id":"t01","service":"t01-site"}
```

Example success (UID and state are observed, not inferred from desired state):

```json
{"ok":true,"op":"service.status","detail":"t01-site observed from systemd","version":"0.1.0","uid":1001,"unit":"t01-site","active_state":"inactive"}
```

The new response fields are optional: `uid` is a JSON unsigned integer/Rust `u32`, `unit` echoes the requested bare unit name (without `.service`), and `active_state` is a lowercase enum: `active`, `reloading`, `inactive`, `failed`, `activating`, `deactivating`, `maintenance`, or `refreshing`. They are omitted for other operations and for errors. Existing clients can ignore them; new clients should deserialize them as optional fields and treat a refused request as unknown observation, never as `inactive`.

The status query uses `systemctl --user show` rather than `is-active`, so `inactive` and `failed` are valid observations rather than command errors. Missing/untrusted Quadlets, aliases, unknown states, and units not loaded from the expected broker Quadlet/generator paths are refused using the existing `ok:false,error` response. A service that was never provisioned does not receive a fabricated status. The app can store the observed `uid` for telemetry and `active_state` for reconciliation; app changes are not included here.

## Validation and scope

Service kinds are restricted to `web`, `database`, `storage`, `git`, and `container`. CPU weights, task counts, and memory values are range-checked; `MemoryHigh` cannot exceed `MemoryMax`. Units are staged, expanded with Quadlet's `-user -dryrun`, and the generated native services are checked with `systemd-analyze verify --user` before replacing the active Quadlet file. `TasksMax` is paired with Quadlet `PidsLimit`, and a static `IOWeight` is emitted.

Quadlet and staging directories are explicitly `root:root 0755`, with root-owned `0644` files, so the target UID can read them without being able to modify them. Target commands start in the tenant home and receive that tenant's actual supplementary groups. Dispatch runs on Tokio's blocking pool; at most 32 connections are active, request lines are capped at 128 KiB (to accommodate escaped environment JSON), socket reads/writes and external commands have 20-second timeouts, and Caddy operations have 10-second timeouts. External command stdout/stderr are nonblocking pipes polled under one shared deadline through both process exit and pipe EOF, with 1 MiB stdout and 128 KiB stderr caps. Retained pipes after the direct child exits still time out. Timeout/output overflow kills the process group and closes captured pipes, without reader threads or joins. Reaping after kill is bounded to 100 ms; a kernel-stuck child is warned about rather than blocking the broker indefinitely.

This is not full production resource governance. Plans now have hard CPU quotas and host CPU/memory reservation admission, but disk and bandwidth quotas, aggregate tenant runtime slices, idempotent jobs/outbox, signed artifacts, and continuous reconciliation remain unimplemented. No filesystem quotas are attempted here. Billing and per-end-user authorization remain app responsibilities; the broker only permits plans explicitly granted to the tenant by root policy. These controls require separate design and host integration tests. Ownership/provenance checks assume an installer-controlled Quadlet root and a trusted user-manager control path; they do not eliminate races with independent administrator or tenant-manager changes, prove workload health, or authenticate an end-user through the shared app socket.


## Generic containers (primary image-driven path)

This path is not bound to an application template. The operator must explicitly
add registry authorities to top-level `allowed_registries` and `container` to an
entitled plan's `allowed_kinds`. The shipped example keeps registry access denied.
The app first calls `tenant.create`, then sends:

```json
{"op":"container.create","tenant_id":"t01","service":"t01-forge-one","plan_code":"pro","container":{"image":"codeberg.org/forgejo/forgejo:17-rootless","environment":{"FORGEJO__server__HTTP_PORT":"3000","FORGEJO__server__ROOT_URL":"${HONE_HTTP_ORIGIN}/"},"container_port":3000,"user":1000,"read_only":false,"volumes":[{"name":"data","target":"/var/lib/gitea"},{"name":"config","target":"/etc/gitea"}]}}
```

The example requires an operator-reviewed policy opt-in; it is documentation,
not an instruction to provision a live service. The broker does not bootstrap an
administrator or assert application readiness. The image default entrypoint/CMD
is retained. `Pull=missing`, a tenant-internal network, a single broker-allocated
loopback TCP publication, keep-id user mapping, all capabilities dropped,
`NoNewPrivileges`, and the resolved plan's CPU/memory/PID envelope are immutable.
There are no command/entrypoint overrides, privileged mode, devices, host network,
engine sockets, or user-selected host paths. Container environment variables
(including `LD_PRELOAD` and `CONTAINER_HOST`) go only into Podman's container env
file, never the broker/engine process environment.

`image` must be fully qualified with an explicit tag or `@sha256:` plus 64 lowercase
hex digits, at most 512 bytes, and use an exactly allowed registry. Schemes,
credentials, whitespace/control characters and command/Quadlet injection are
refused. `container_port` must be positive; `user` defaults to 1000 and must be
1–65535. `read_only` defaults false, `environment` and `volumes` default empty.
Environment has at most 32 ASCII identifier keys (64 bytes each), 4096 bytes per
value, and 16 KiB total rendered `KEY=value` lines. Values cannot contain control
characters. After allocating the host port, the broker replaces only the reserved
`${HONE_HOST_PORT}` and `${HONE_HTTP_ORIGIN}` placeholders in environment values,
with the decimal port and `http://127.0.0.1:<port>` respectively. Replacement works
for an entire value or within a value, so the Forgejo example above produces a
`FORGEJO__server__ROOT_URL` such as `http://127.0.0.1:30001/` when port 30001 is
allocated. These placeholders are intentionally reserved; all other dollar-prefixed
text, including `$HOME` and unknown `${...}` forms, remains literal. There is no
shell expansion or evaluation. The 4096-byte per-value and 16 KiB total limits are
checked again after replacement, before metadata or environment files are committed;
violations return the same redacted invalid-specification error. Volume names are
1–32 lowercase identifier bytes, starting with a letter; targets are normalized
absolute container paths, without overlapping
ancestors/descendants, traversal, root, `/proc`, `/sys`, `/dev`, `/run`, `/var/run`
control trees, or `.sock` targets. At most eight distinct managed volumes are
accepted. Fixed template names (`site`, `web`, `db`, `database`, `vault`, `files`,
`storage`, `git`, `forgejo`) are reserved, as are all previously committed generic
names. Generic image/spec changes require a new unit name, not `limits.set`.

The HoneNode generic configuration page is read-only: it cannot edit environment
values after creation/inspection. Set application environment correctly on the
first create; full service/environment administration is future work. In
particular, `FORGEJO__server__ROOT_URL=${HONE_HTTP_ORIGIN}/` delegates the actual
loopback URL to the broker's allocated port. The app submits the literal placeholder
and the broker resolves it before writing the root-owned env file; the UI cannot
infer or safely patch it after inspect. A stable operator-routed external URL known
before creation remains an alternative. See `../honenode/CONTAINERS.md` for the manual
Forgejo guide and browser-only optional secret generation.

Create and `container.inspect` return optional `container` with exactly:

```json
{"name":"t01-forge-one","image":"codeberg.org/forgejo/forgejo:17-rootless","host_port":30000,"container_port":3000,"user":1000,"read_only":false,"volumes":[{"name":"data","target":"/var/lib/gitea"},{"name":"config","target":"/etc/gitea"}],"environment_keys":["FORGEJO__server__HTTP_PORT","FORGEJO__server__ROOT_URL"]}
```

`container.inspect` takes only `tenant_id` and `service`. `service.inspect` also
returns this field for generic units. `service.status` retains the usual observed
`uid`, `unit`, and `active_state` plus safe container info. Nothing in this object
contains environment values, host paths, or registry credentials; the app can
persist it directly. Create admits CPU/memory/service counts **before writes**,
requires inactive/no pending job, stages/expands/verifies the Quadlet, durably
commits metadata/environment/unit, rechecks canonical bytes and cached limits,
then queues a nonblocking start. Success is queued intent, not health.

Metadata is `$HONE_ETC/containers/<unit>.json`, root:root `0600`, with safe info
and the canonical unit/reservation, never cleartext environment. Environment
values exist only in a separate root:<tenant-group> `0640` `<unit>.env`. The
metadata directory is root `0711` under root `0755` HONE_ETC; ancestors must be
trusted and permit tenant traversal. Managed volume sources are exclusively
`$HONE_TENANT_DATA/<tenant>/containers/<unit>/<name>`, tenant-owned `0750`, under
root-controlled container/unit directories. Descriptor-relative, no-symlink
walks protect root provisioning operations. No broker metadata or engine state
is mounted. Activation revalidates metadata, env ownership/mode/keys, managed
sources, policy registry/plan, and every rendered unit byte.

Ports are selected from 30000–39999, excluding committed publications and all
metadata reservations, then checked by an OS loopback bind. Metadata discovery
is independent of the tenant allowlist and NSS. Deletes synchronously stop and
remove only the live unit, retaining volumes, env and metadata as permanent
name/port/CPU/memory tombstones. Partial commits also fail closed and retain the
reservation; operators must review/reconcile them rather than retrying the same
name or deleting reservation files blindly. Existing legacy committed port
layouts that cannot be interpreted conservatively refuse allocation.

Start/restart/resume/status/stop/suspend/delete/log fetch use the existing
root-unit and systemd-generator provenance gates. Stops remain possible after
policy/registry revocation, provided provenance and retained files are intact.
Logs are bounded, never copied into broker audit reasons, and exact current
environment values are redacted from generic log responses. **An arbitrary image
can itself log a secret or an encoded/transformed version to the host journal;
this is not a guarantee against application-originated secret logging.** Operators
must apply journal access/retention policy and choose trusted images.

Generic `edge.reload` requires exactly `<unit>.<HONE_TENANT_DOMAIN>` and derives
its loopback upstream from metadata. An optional supplied `upstream` must match;
no arbitrary host is accepted. Fixed-template vhost checks are unchanged. This
does not install/enable Caddy. Ports are checked at allocation, not held open
while systemd pulls/starts: unrelated external listeners can still race the
start and produce a failed observed state. The broker is the serialized writer;
independent root/tenant-manager changes require operator coordination.

Validation is offline unit tests plus local Quadlet dry-run expansion. No account,
container, live unit, root configuration, image pull, Caddy reload, or HTTP
service is provisioned by the tests. End-to-end image startup, SELinux/mount
behavior, image-specific onboarding, runtime health and edge routing still need
the operator's separate Forgejo final test. Disk/bandwidth quotas and public
registry credentials are intentionally not implemented here.

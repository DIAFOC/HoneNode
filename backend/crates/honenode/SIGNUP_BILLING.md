# Sign-up, trials and top-up keys (0.8.x deltas)

This note documents the four gaps closed on top of the 0.8.0 evaluation release:
first-run operator credentials, tenant self-service sign-up, the 7-day trial on
the basic plan, and top-up keys for plan time. It also records the trusted
registry index and the DNS/TOTP egress verdict.

## 1. First-run operator credentials (shown once)

Before this change the operator account only came from a `HONE_OPERATOR_PASSWORD`
environment import (the `.env`/`bootstrap.env` flow). Now the app can bootstrap
itself:

* On startup, if no operator account exists **and** `HONE_OPERATOR_PASSWORD` is
  unset, the app generates a 12-digit account number (Luhn check digit) and a
  24-character password, and creates the operator account.
* The credentials are surfaced **exactly twice, once each**:
  1. one `warn` log line on stdout (the journal / `podman logs` stream), and
  2. a mode-`0600` file next to the registry
     (`<registry dir>/bootstrap-credentials.txt`, override with
     `HONE_BOOTSTRAP_FILE`), written with `O_EXCL` so a second boot never
     overwrites or regenerates it.
* The log line instructs the administrator to save them in a secure
  environment; they are never shown again. The password is stored only as an
  argon2id hash in the registry.
* The env-import path is unchanged: when `HONE_OPERATOR_PASSWORD` is set, the
  app behaves exactly as before (the lab harness `run_native.py` keeps its
  `bootstrap.env` flow).

## 2. Tenant self-service sign-up

The login panel is shared between the operator and tenants; it now links to
`/signup`:

1. `GET /signup` renders a name-only form bound to a one-time challenge cookie
   (the same `login_challenges` mechanism the login form uses).
2. `POST /signup` consumes the challenge, applies the sign-up rate limits
   (`signup:global` 20/h, `signup:ip:<addr>` 5/h in `login_limits`), and then:
   * allocates the first free tenant slug from the node allowlist
     (`HONE_TENANTS`; the broker refuses tenants outside the same list, so the
     allowlist is the shared contract);
   * creates the account (`role = tenant`, generated number + 24-char password,
     argon2id hash in SQLite);
   * creates the tenant row and a **trial subscription** on `starter` with
     `paid_through = now + HONE_TRIAL_DAYS` (default 7);
   * requests `tenant.create` from the broker (Unix account, subuids, linger,
     network). A broker failure does not roll back the account: the job and the
     audit event record the failure and the credentials page shows the note.
3. The response page shows the account number and password **once**, with the
   save-them-now instruction, and links back to `/login`.

There is still no email in the circuit (§4.7 of SN_4.1): recovery remains
operator-mediated, and the account number is the bearer identifier.

## 3. The trial: how the registry counts time

The subscription row is the trial clock:

* Provisioning caps every new service at `min(now + service_days, paid_through)`
  when a subscription exists (`provisioning::subscription_cap`). Without a
  subscription (operator-created tenants) nothing changes.
* An expired subscription (`paid_through <= now`) refuses new provisioning with
  "el plan expiró; canjea una clave de recarga" until a key is redeemed.
* Self-service tenants are bound to their subscription's plan: a new service
  must match it (`provisioning::require_subscription_plan`), so the trial
  cannot jump to a larger envelope. Operator-created tenants are unrestricted.
* The existing lifecycle machinery (grace → suspend → retention) keeps working
  off `services.expires_at`; the trial simply bounds it.

The demo plan shapes live in `crates/honed/policy.selfservice.example.json`:
`starter` = 2 services, 7 days; `pro` = 4 services, 30 days; a `t*` wildcard
entitles every allowlisted tenant that has no exact entry. The wildcard is a
policy-schema feature (exact entries always win), covered by broker tests.

## 4. Top-up keys (the "steam key" workaround)

No payment gateway yet, so plan time is granted with prepaid keys:

* **Generation (operator panel, `/operator/keys`):** pick plan code, days
  (1–3650) and count (1–50). Codes are `HN-XXXX-XXXX-XXXX-XXXX`, Crockford
  base32 (80 bits). The full codes are shown **once** on the confirmation page;
  the registry stores only a SHA-256 of the normalized code plus a 4-character
  prefix for the listing. A leaked database therefore does not leak redeemable
  keys.
* **Redemption (tenant portal):** the portal shows plan, `paid_through` and
  remaining days, plus the redeem form. `topup::redeem` claims the key
  atomically (`UPDATE … WHERE claimed_at IS NULL`) and, in one transaction:
  * extends **every** non-deleted service additively (same rule as
    `registry::extend_service`: base = max(expires_at, now) + days);
  * returns suspended services to `stopped` so the tenant can start them again
    (suspension only ever stopped the unit; the reconciler performs no host
    writes);
  * moves the subscription to the key's plan and extends `paid_through`
    additively from max(now, current paid_through);
  * returns a grace/suspended tenant to `active` while the new expiry is future.
* Reuse is refused (`Conflict`), unknown codes are `NotFound`, malformed input
  is `BadRequest`. Every attempt is audited (`topup.redeem`), as is generation
  (`topup.generate`).

Keys do not mutate broker policy: entitlements remain root policy. A key whose
plan is not entitled for the tenant will record the subscription but
provisioning that plan still fails closed at the broker — generate keys for
plans the tenant's policy allows (`starter`/`pro` under the self-service
example).

## 5. Trusted registry index

`/portal/registry` ("Registro de confianza") lists the curated images with
upstream registry links. It is **documentation, not authorization**: every
deployment still validates against the live root catalogue. Verified against
the upstream publishers on 2026-11-17:

| Workload | Reference | Note |
|---|---|---|
| Blog estático (demo) | `docker.io/nginxinc/nginx-unprivileged:1.27-alpine` | platform web template |
| BD de la tienda (demo) | `docker.io/library/postgres:17-alpine` | platform database template |
| Bóveda de medios (demo) | `docker.io/filebrowser/filebrowser:v2.63.23` | archived upstream, operator decision pending |
| Forja git (demo) | `codeberg.org/forgejo/forgejo:17-rootless` | SSH on the tenant's dedicated port |
| whoami | `docker.io/traefik/whoami:v1.12.0` | proven in the lab |
| MariaDB | `docker.io/library/mariadb:11` | pin the minor tag for production |
| Bruno CLI ("brunoapi") | `docker.io/usebruno/cli` | Docker Verified Publisher; runs a collection and exits, UID 1000 |
| OWASP ZAP | `ghcr.io/zaproxy/zaproxy:stable` | actively published; needs reachability to the target |
| CoreDNS | `docker.io/coredns/coredns` | authoritative-only; see the egress verdict |
| 2FAuth | `docker.io/2fauth/2fauth` | TOTP manager; inbound-only, UID 1000, SQLite |

## 6. DNS and 2FA-TOTP: egress verdict

Rule: tenant containers have **inbound** (loopback publication through the
edge) but **no outbound-initiated** traffic (§3.2 rule 6).

* **CoreDNS — viable with constraints.** An *authoritative* server for a zone
  answers queries directed at it: inbound only, no egress needed. A *recursive
  resolver* initiates upstream queries and would need an egress exception.
  Publishing UDP/53 publicly also contradicts "only Caddy holds public ports";
  the model-compatible exposure is DNS-over-HTTPS through the edge (CoreDNS
  supports RFC 8484) or loopback-only in the lab. Included in the index with
  that note; not a catalogue template.
* **2FAuth — viable as-is.** A web app that validates/generates TOTP codes:
  pure inbound HTTP, no outbound calls required, runs as UID 1000 with SQLite
  storage. Fits the generic-container path (`/portal/containers/new`) without
  any policy change. Included in the index.
* The bitcoin-node class of workload stays excluded: it is the outbound
  (peer-to-peer) traffic that the model blocks by construction, not the
  service type.

## 7. Admission model: soft budgets, not a brick wall

Provisioning used to refuse when the *sum of hard limits* exceeded the host or
tenant budget. That made the broker a brick wall: four pro services (4 × 150%
CPU, 4 × 1536M) could not fit a 300% / 6G budget even though the kernel would
simply throttle them. Admission now follows the requests-vs-limits model:

* **CPU**: `CPUQuota` throttles each cgroup and never kills it. The policy's
  `host_cpu_quota_percent` is the **soft** budget (a warning above it); admission
  refuses only beyond `HONE_ADMISSION_OVERCOMMIT_PERCENT` (default 200% of the
  soft budget).
* **Memory**: `MemoryHigh` reclaims and `MemoryMax` caps each cgroup. The host
  budget and the tenant ceiling are soft; the hard ceiling is the same
  overcommit factor.
* **Orphaned generic-container metadata** (a metadata record whose Quadlet unit
  no longer exists) is ignored by admission and quota, so a removed or
  never-started service cannot poison a tenant's service count.

## 7b. Multiple instances of one kind

A tenant is no longer limited to one unit of each fixed template. The old
`(tenant_id, kind)` partial index and the "one unit per tenant kind" admission
rule are gone. The app allocates the next free instance before reserving:
instance 1 keeps the legacy `t91-site` / `18091` / `.../site` layout, instance 2
uses `t91-site-2` / `18191` / `.../site-2/site`, and so on. Unit names, ports
and volumes stay exclusive per tenant, and the plan's `max_services` plus the
host budgets remain the real limits. The demo policy's `pro` plan now allows six
services so a tenant can run, for example, two nginx instances and three
PostgreSQL instances during a presentation.

## 7c. Web content and the demo blog

The nginx template is no longer a placeholder-only server. A tenant can open
**Contenido** on a web service and write text files under `site/` (`index.html`,
`css/site.css`, `js/app.js`, ...). The broker resolves every component with
`O_NOFOLLOW`, writes through a temporary sibling and renames it into place,
chowns it to the tenant and labels it for containers; nginx keeps serving a
read-only mount. The Express blog in this repository is **not** static, so it
runs as a generic container instead: `packaging/fedora/load_blog_image.sh`
builds `localhost/honenode/seclab:1.0` and loads it into the tenant's rootless
store, and the tenant creates it from **Nuevo servicio** (the generic container
form) with image `localhost/honenode/seclab:1.0`, port 3000, UID 1000,
`PORT=3000` and `NODE_ENV=production`. The blog is not a catalogue card; the
image reference is entered by the tenant.

## 8. Two product fixes surfaced by the Rocky smoke

Running the full flow inside a Rocky 10 container (systemd as PID 1) exposed two
real defects, both fixed in `honed`:

1. **`systemd-analyze verify --user` killed the tenant's live user bus.** The
   verification manager that verify spawns rebinds the sockets under
   `XDG_RUNTIME_DIR` and leaves them dead when it exits (systemd 257), so every
   later `systemctl --user` call for that tenant failed with `Connection
   refused` and provisioning broke. `validate_unit` now runs verify with a
   throwaway runtime directory (`/tmp/honed-verify-…`, tenant-owned, `0700`,
   removed afterwards): verification is static analysis and must never touch
   the running manager.
2. **Subuid allocation was not configurable.** `ensure_subid` hardcoded a
   100000 base, which cannot fit inside the subuid window a nested environment
   delegates (distrobox, podman-in-podman). The base is now `HONE_SUBID_BASE`
   (default `100000`). The smoke sets `1`-based windows via the wrapper.

## 8b. Boot/restart wizard and OS-tagged bundles

The native Fedora lab has a wizard, `packaging/fedora/lab.py`. It starts the
broker first (the app launcher deliberately does not), waits for the real
socket and `/healthz`, and then shows the first-run operator credentials once
and offers to delete the static bootstrap file. `status`, `stop`, `reset`,
`credentials`, `url` and an interactive menu are included; `reset` clears stale
units, state and tenant data before starting fresh. The broker remains
intentionally disabled at boot, so a reboot is recovered with
`python3 packaging/fedora/lab.py start` instead of a manual `systemctl` hunt.

Packaging is now explicitly per OS: `packaging/os/fedora/build.sh` builds and
tags a Fedora bundle, and `packaging/os/rocky/build.sh` boots Rocky 10, builds
there, and tags a Rocky bundle. `manifest.json` records `"os"` and the ELF
libc/interpreter. One tarball is no longer presented as portable across both.

## 9. Acceptance notes

* `cargo test`: 107 broker + 76 app tests, including sign-up (one-time
  credentials, trial subscription, allowlist exhaustion), keys (additive
  redemption, reuse refusal, hash-only storage), the subscription cap, static
  site writes and multiple instances of one kind.
* `python3.14 -m unittest discover -s backend/crates/honenode/tests`: 33 tests,
  including the production SQL executed against SQLite (the login-limit query
  now lives in `auth::limit_bucket`).
* `python3.14 -m unittest discover -s packaging/tests`: 53 tests, including the
  OS-tagged bundle builder (Fedora/Rocky tag, bad target, bad suffix).
* Rocky smoke (`packaging/rocky/run_smoke_podman.sh`, Rocky 10 + systemd in a
  privileged container): the benchmark signs up a tenant, redeems a pro key,
  then provisions **all four demo services on that single tenant** — static
  blog (nginx, HTTP 18001), shop database (PostgreSQL, TCP 15431), media vault
  (File Browser, HTTP 19001) and git forge (Forgejo, HTTP 20001 + SSH 22001) —
  writes `index.html` through the tenant panel and verifies nginx serves it,
  provisions a **second nginx (18101) and a second PostgreSQL (15531)**, then
  deletes the first web service and re-provisions it, proving the
  kind/port/volume reservation is released. The wrapper exports `HONE_SUBID_BASE=2000`,
  `HONE_SUBID_COUNT=63536`, `HONE_TENANT_MEMORY_MAX=8G`, disables `login.defs`
  subuid auto-allocation, forces the vfs storage driver and preloads Docker Hub
  images through the public mirror; those are nested-container accommodations,
  not product settings.

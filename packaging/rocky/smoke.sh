#!/bin/bash
# =============================================================================
# HoneNode · Rocky Linux smoke test
#
# Acceptance for the 0.8.x gaps, designed for a distrobox Rocky box (or any
# Rocky container with systemd as PID 1).  It exercises the real product path:
#
#   first-run operator credentials (shown once) → shared login panel →
#   tenant sign-up (credentials shown once, trial subscription) → tenant
#   login → portal → operator generates top-up keys → tenant redeems one →
#   (full mode) tenant provisions the fixed web template and the service
#   answers HTTP on its loopback port.
#
# Usage (inside Rocky, root):
#   sudo bash packaging/rocky/smoke.sh [--app-only]
#
# From any host with podman (boots Rocky 10 with systemd and runs the smoke):
#   bash packaging/rocky/run_smoke_podman.sh
#
# Inside a nested container (distrobox, podman-in-podman) the outer user
# namespace only delegates a narrow subuid window; the wrapper exports
# HONE_SUBID_BASE=1 so tenant allocations fit it. On a bare-metal Rocky host
# the default (100000) applies and no flag is needed.
#
# App-only skips tenant service provisioning (no nested podman required);
# full mode provisions the nginx template through the real broker and curls
# the published loopback port — that is the "deployed and accessed" gate.
# =============================================================================
set -euo pipefail

APP_ONLY=0
BUILD_ONLY=0
for arg in "$@"; do
  case "$arg" in
    --app-only) APP_ONLY=1 ;;
    --build-only) BUILD_ONLY=1 ;;
    *) echo "usage: smoke.sh [--app-only|--build-only]" >&2; exit 2 ;;
  esac
done

# ── phase 0: identity ────────────────────────────────────────────────────────
. /etc/os-release
if [[ "${ID:-}" != "rocky" && "${ID_LIKE:-}" != *"rocky"* ]]; then
  echo "FAIL: this smoke must run on Rocky Linux (got ID=${ID:-unknown})" >&2
  exit 1
fi
MODE=full
[[ "$APP_ONLY" == 1 ]] && MODE=app-only
[[ "$BUILD_ONLY" == 1 ]] && MODE=build-only
echo "── Rocky ${VERSION_ID:-?} smoke ($MODE) ──"

if [[ $EUID -ne 0 ]]; then
  echo "FAIL: run as root inside the distrobox/container (the broker needs host authority)" >&2
  exit 1
fi

REPO=${HONENODE_REPO:-/workspace}
BACKEND="$REPO/backend"
STATE=/var/lib/honenode-smoke
ETC=/etc/honenode-smoke
RUNDIR=/run/honenode-smoke
SOCKET="$RUNDIR/broker.sock"
BIND=127.0.0.1:8080
BASE=http://127.0.0.1:8080
SMOKE_USER=honesmoke
TENANTS=t01,t02,t03,t04,t05,t06
DB_IMAGE="docker.io/library/postgres:17.11-alpine"
DB_PASSWORD_ONE="BenchmarkDatabase2026"
DB_PASSWORD_TWO="BenchmarkDatabase2026b"

PASS=0; FAIL=0
ok()   { echo "  ✓ $1"; PASS=$((PASS+1)); }
bad()  { echo "  ✗ $1"; FAIL=$((FAIL+1)); }
phase(){ echo; echo "── $1 ──"; }

# ── phase 1: prerequisites ──────────────────────────────────────────────────
phase "Prerequisites"
dnf -y install curl jq sqlite podman passt shadow-utils systemd-udev >/dev/null 2>&1
command -v curl >/dev/null && ok "curl" || bad "curl"
command -v jq >/dev/null && ok "jq" || bad "jq"
command -v sqlite3 >/dev/null && ok "sqlite3" || bad "sqlite3"
command -v loginctl >/dev/null && ok "loginctl (systemd-udev)" || bad "loginctl"
# Container images ship logind masked; linger (and therefore the tenant user
# managers) needs it. A real Rocky host has it running already.
if systemctl unmask systemd-logind >/dev/null 2>&1 && systemctl start systemd-logind >/dev/null 2>&1; then
  ok "systemd-logind active (container unmask)"
else
  systemctl is-active systemd-logind >/dev/null && ok "systemd-logind active" || bad "systemd-logind"
fi

if [[ -n "${HONED_BIN:-}" && -n "${HONENODE_BIN:-}" ]]; then
  ok "using prebuilt binaries"
else
  if ! command -v cargo >/dev/null; then
    dnf -y install rust cargo >/dev/null 2>&1
  fi
  command -v cargo >/dev/null && ok "cargo $(cargo --version | awk '{print $2}')" || bad "cargo"
fi

# ── phase 2: build ───────────────────────────────────────────────────────────
phase "Build"
if [[ -n "${HONED_BIN:-}" && -n "${HONENODE_BIN:-}" ]]; then
  HONED="$HONED_BIN"; APP="$HONENODE_BIN"
else
  (cd "$BACKEND" && cargo build --release --quiet) || { echo "FAIL: cargo build" >&2; exit 1; }
  HONED="$BACKEND/target/release/honed"
  APP="$BACKEND/target/release/honenode"
fi
[[ -x "$HONED" ]] && ok "honed binary" || bad "honed binary"
[[ -x "$APP" ]] && ok "honenode binary" || bad "honenode binary"

# Build-only mode is used by packaging/os/rocky/build.sh: copy the Rocky-built
# binaries back to the mounted repository and stop before touching host state.
if [[ "$BUILD_ONLY" == 1 ]]; then
  mkdir -p "$REPO/packaging/os/dist/rocky"
  cp "$HONED" "$REPO/packaging/os/dist/rocky/honed"
  cp "$APP" "$REPO/packaging/os/dist/rocky/honenode"
  echo "Rocky binaries copied to $REPO/packaging/os/dist/rocky"
  exit 0
fi

# ── phase 3: layout and policy ───────────────────────────────────────────────
phase "Layout and policy"
pkill -f 'release/honed' 2>/dev/null || true
pkill -f 'release/honenode' 2>/dev/null || true
rm -rf "$STATE" "$ETC" "$RUNDIR"
mkdir -p "$STATE/registry" "$ETC/caddy" "$RUNDIR" /etc/containers/systemd/users
install -m 0644 "$BACKEND/crates/honed/policy.selfservice.example.json" "$ETC/policy.json"
ok "self-service policy installed (starter 2×7d, pro 6×30d, t* wildcard)"

id -u "$SMOKE_USER" >/dev/null 2>&1 || useradd --create-home --shell /bin/bash "$SMOKE_USER"
# The broker binds its socket 0660 root:root; the app connects through it.
usermod -aG root "$SMOKE_USER"
# Only the app's registry subtree is smoke-user-owned: the broker validates
# that every ancestor of its unit/data paths stays root-owned and not
# group/other-writable.
chown "$SMOKE_USER":"$SMOKE_USER" "$STATE/registry"
# In nested environments the wrapper sets HONE_SUBID_BASE to fit the outer
# user namespace; useradd's own sequential subuid allocation (login.defs)
# must then be disabled so the broker's allocation is the one that sticks.
if [[ -n "${HONE_SUBID_BASE:-}" ]]; then
  # login.defs is first-match-wins: edit the existing entries in place.
  sed -i 's/^SUB_UID_COUNT.*/SUB_UID_COUNT   0/; s/^SUB_GID_COUNT.*/SUB_GID_COUNT   0/' /etc/login.defs
  # Nested podman cannot use overlayfs over overlayfs: force vfs. The distro
  # vendor drop-in pins overlay and outranks /etc, so it must be neutralized.
  cp /usr/share/containers/storage.conf /etc/containers/storage.conf
  sed -i 's/^#*[[:space:]]*driver = .*/driver = "vfs"/' /etc/containers/storage.conf
  mv /usr/share/containers/storage.conf.d/00-vendor.conf \
     /usr/share/containers/storage.conf.d/00-vendor.conf.disabled 2>/dev/null || true
fi

# ── phase 4: broker up ───────────────────────────────────────────────────────
phase "Broker (honed)"
env HONE_BROKER_SOCKET="$SOCKET" \
    HONE_POLICY_FILE="$ETC/policy.json" \
    HONE_TENANTS="$TENANTS" \
    HONE_TENANT_DATA="$STATE/srv" \
    HONE_ETC="$ETC" \
    HONE_CADDY_DIR="$ETC/caddy" \
    HONE_TENANT_DOMAIN="smoke.lab" \
    HONE_SERVICE_ACCESS=loopback \
    HONE_SUBID_BASE="${HONE_SUBID_BASE:-100000}" \
    HONE_TENANT_MEMORY_MAX="${HONE_TENANT_MEMORY_MAX:-8G}" \
    HONE_IMAGE_NGINX="docker.io/nginxinc/nginx-unprivileged:1.31.7-alpine@sha256:b5047b9b76845eac124358373c6852377c52e4b05834a47647e955dd91cb8f1e" \
    HONE_IMAGE_POSTGRES="$DB_IMAGE" \
    "$HONED" >>"$STATE/honed.log" 2>&1 &
for _ in $(seq 1 50); do
  [[ -S "$SOCKET" ]] && break
  sleep 0.2
done
[[ -S "$SOCKET" ]] && ok "broker socket present" || { bad "broker socket"; tail -5 "$STATE/honed.log"; }

# ── phase 5: app up (first run generates operator credentials) ───────────────
phase "App first run"
runuser -u "$SMOKE_USER" -- env \
    HONE_BIND="$BIND" \
    HONE_BROKER_SOCKET="$SOCKET" \
    HONE_REGISTRY="$STATE/registry/registry.db" \
    HONE_TENANT_DATA="$STATE/srv" \
    HONE_TENANTS="$TENANTS" \
    HONE_STATIC_DIR="$BACKEND/static" \
    HONE_SECURE_COOKIES=false \
    HONE_SERVICE_ACCESS=loopback \
    HONE_LOG_FORMAT=plain \
    HONE_THEME=neutral \
    HONE_API_TOKENS= \
    HONE_TRIAL_DAYS=7 \
    "$APP" >>"$STATE/app.log" 2>&1 &
for _ in $(seq 1 50); do
  curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done
curl -fsS "$BASE/healthz" | grep -q ok && ok "healthz" || { bad "healthz"; tail -15 "$STATE/app.log"; }

grep -q "first-run operator credentials" "$STATE/app.log" && \
  ok "operator credentials announced once in the log" || bad "operator credentials log"
BOOTSTRAP_FILE="$STATE/registry/bootstrap-credentials.txt"
if [[ -f "$BOOTSTRAP_FILE" ]] && [[ "$(stat -c %a "$BOOTSTRAP_FILE")" == "600" ]]; then
  ok "bootstrap file written with mode 0600"
else
  bad "bootstrap file mode/content"
fi
OP_NUMBER=$(sed -n 's/^HONE_OPERATOR_ACCOUNT=//p' "$BOOTSTRAP_FILE")
OP_PASSWORD=$(sed -n 's/^HONE_OPERATOR_PASSWORD=//p' "$BOOTSTRAP_FILE")
[[ "$OP_NUMBER" =~ ^[0-9]{12}$ ]] && ok "operator account number generated" || bad "operator number"

# ── helpers ──────────────────────────────────────────────────────────────────
# One GET /signup issues the challenge cookie and the matching csrf together.
challenge() { # jar -> prints "cookie<TAB>csrf"
  local jar="$1" html csrf cookie
  html=$(curl -fsS -c "$jar" "$BASE/signup")
  csrf=$(echo "$html" | grep -o 'name="csrf" value="[a-f0-9]*"' | head -1 | cut -d'"' -f4)
  cookie=$(awk '/hone_login/ && $6 != "" {print $6"="$7; exit}' "$jar")
  printf '%s\t%s' "$cookie" "$csrf"
}

login_session() { # number password jar -> session cookie (fails without one)
  local number="$1" password="$2" jar="$3" pair csrf cookie session
  rm -f "$jar"
  pair=$(challenge "$jar") || return 1
  cookie=${pair%%$'\t'*}; csrf=${pair##*$'\t'}
  curl -fsS -b "$jar" -c "$jar" \
    -d "number=$number" -d "password=$password" -d "csrf=$csrf" \
    "$BASE/login" >/dev/null || return 1
  session=$(awk '/hone_session/ && $6 != "" {print $6"="$7; exit}' "$jar")
  [[ -n "$session" ]] && { printf '%s' "$session"; return 0; }
  return 1
}

form_csrf() { # html -> csrf token (per-session, shared by every form)
  echo "$1" | grep -o 'name="csrf" value="[a-f0-9]*"' | head -1 | cut -d'"' -f4
}

# ── phase 6: tenant sign-up ──────────────────────────────────────────────────
phase "Tenant sign-up (credentials shown once)"
JAR=$(mktemp)
pair=$(challenge "$JAR")
cookie=${pair%%$'\t'*}; csrf=${pair##*$'\t'}
SIGNUP_HTML=$(curl -fsS -b "$JAR" -d "name=Velas+de+la+Casa" -d "csrf=$csrf" "$BASE/signup")
echo "$SIGNUP_HTML" | grep -q "una sola vez" && ok "credentials page shown once" || bad "credentials page"
T_NUMBER=$(echo "$SIGNUP_HTML" | sed -n 's/.*Número de cuenta<\/dt><dd class="mono">\([0-9]*\)<\/dd>.*/\1/p' | head -1)
T_PASSWORD=$(echo "$SIGNUP_HTML" | sed -n 's/.*Contraseña<\/dt><dd class="mono">\([^<]*\)<\/dd>.*/\1/p' | head -1)
[[ "$T_NUMBER" =~ ^[0-9]{12}$ ]] && ok "tenant account number generated" || bad "tenant number"
[[ -n "$T_PASSWORD" ]] && ok "tenant password generated" || bad "tenant password"
sqlite3 "$STATE/registry/registry.db" "SELECT paid_through IS NOT NULL FROM subscriptions" \
  | grep -q 1 && ok "trial subscription stored in SQLite" || bad "trial subscription"

SESSION=$(login_session "$T_NUMBER" "$T_PASSWORD" "$JAR") && \
  ok "tenant logs in with generated credentials" || bad "tenant login"
PORTAL_HTML=$(curl -fsS -b "$JAR" "$BASE/portal")
echo "$PORTAL_HTML" | grep -q "Plan y tiempo" && ok "portal shows plan and time" || bad "portal plan card"
TENANT_CSRF=$(form_csrf "$PORTAL_HTML")

# ── phase 7: operator keys ───────────────────────────────────────────────────
phase "Operator generates top-up keys"
OJAR=$(mktemp)
login_session "$OP_NUMBER" "$OP_PASSWORD" "$OJAR" >/dev/null && ok "operator logs in" || bad "operator login"
KEYS_HTML=$(curl -fsS -b "$OJAR" "$BASE/operator/keys")
KEYS_CSRF=$(form_csrf "$KEYS_HTML")
CREATED_HTML=$(curl -fsS -b "$OJAR" -d "csrf=$KEYS_CSRF" -d "plan_code=pro" -d "days=30" -d "count=2" "$BASE/operator/keys")
echo "$CREATED_HTML" | grep -q "una sola vez" && ok "keys shown once" || bad "keys shown once"
KEY=$(echo "$CREATED_HTML" | grep -o 'HN-[A-Z0-9]\{4\}-[A-Z0-9]\{4\}-[A-Z0-9]\{4\}-[A-Z0-9]\{4\}' | head -1)
[[ -n "$KEY" ]] && ok "key code parsed ($KEY)" || bad "key code"
sqlite3 "$STATE/registry/registry.db" "SELECT COUNT(*) FROM topup_keys WHERE claimed_at IS NULL" \
  | grep -q 2 && ok "only hashes stored (2 unclaimed)" || bad "key rows"

# ── phase 8: redemption ──────────────────────────────────────────────────────
phase "Tenant redeems a key"
REDEEM_HTML=$(curl -fsSL -b "$JAR" -d "csrf=$TENANT_CSRF" -d "code=$KEY" "$BASE/portal/redeem")
echo "$REDEEM_HTML" | grep -q "días de plan añadidos" && ok "redemption accepted (+30 days)" || bad "redemption"
curl -fsSL -b "$JAR" -d "csrf=$TENANT_CSRF" -d "code=$KEY" "$BASE/portal/redeem" | grep -q "ya fue canjeada" && \
  ok "reuse refused" || bad "reuse refused"

if [[ $APP_ONLY -eq 1 ]]; then
  phase "Summary"
  echo "PASS=$PASS FAIL=$FAIL (app-only)"
  exit $(( FAIL > 0 ))
fi

# ── phase 9: the four demo services on one pro tenant ────────────────────────
phase "Benchmark: four demo services on one pro tenant"
# Container-only accommodations (a real Rocky host needs none of this): the
# first user-manager instance spawned via linger can end up without live
# listeners, nested rootless podman needs newuidmap's caps, and the tenant's
# subuid window must cover the full container uid space (images contain files
# owned by nobody=65534) while excluding the tenant's own uid.
U=$(id -u t01)
python3 - "$U" <<'PY'
import sys
uid = int(sys.argv[1])
ranges = []
if uid > 1:
    ranges.append((1, uid - 1))
ranges.append((uid + 1, 65536 - uid))
for name in ("subuid", "subgid"):
    path = "/etc/" + name
    lines = [l for l in open(path).read().splitlines() if not l.startswith("t01:")]
    for start, length in ranges:
        lines.append(f"t01:{start}:{length}")
    open(path, "w").write("\n".join(lines) + "\n")
PY
systemctl stop "user@$U.service" 2>/dev/null || true
pkill -u t01 2>/dev/null || true
rm -rf /home/t01/.local/share/containers
systemctl start "user@$U.service"
for _ in $(seq 1 40); do
  STATE_OUT=$(runuser -u t01 -- env XDG_RUNTIME_DIR="/run/user/$U" timeout 3 systemctl --user is-system-running 2>/dev/null || true)
  [[ "$STATE_OUT" == running || "$STATE_OUT" == degraded ]] && break
  sleep 0.5
done
[[ "$STATE_OUT" == running || "$STATE_OUT" == degraded ]] && ok "tenant user manager bus reachable" || bad "tenant user manager bus"
ls -l /usr/bin/newuidmap | grep -q -- "-rwsr" || setcap cap_setuid+ep /usr/bin/newuidmap 2>/dev/null || chmod u+s /usr/bin/newuidmap
ls -l /usr/bin/newgidmap | grep -q -- "-rwsr" || setcap cap_setgid+ep /usr/bin/newgidmap 2>/dev/null || chmod u+s /usr/bin/newgidmap
ok "tenant user manager and nested-podman caps ready"
# Docker Hub rate-limits unauthenticated pulls; preload the Docker Hub images
# through the public mirror and tag them under the approved references
# (nested-only). Codeberg (Forgejo) is not rate-limited.
for image in \
  docker.io/nginxinc/nginx-unprivileged:1.31.7-alpine \
  docker.io/library/postgres:17.11-alpine \
  docker.io/filebrowser/filebrowser:v2.63.23; do
  if runuser -u t01 -- env XDG_RUNTIME_DIR="/run/user/$U" podman image exists "$image" 2>/dev/null; then
    continue
  fi
  if runuser -u t01 -- env XDG_RUNTIME_DIR="/run/user/$U" podman pull -q "mirror.gcr.io/${image#docker.io/}" >/dev/null 2>&1; then
    runuser -u t01 -- env XDG_RUNTIME_DIR="/run/user/$U" podman tag "mirror.gcr.io/${image#docker.io/}" "$image"
    ok "preloaded $image through the public mirror"
  else
    ok "$image will be pulled from Docker Hub"
  fi
done

# provision <kind> [extra form fields...] -> prints the redirect target
provision() {
  local kind="$1"; shift
  curl -fsS -b "$JAR" -o /dev/null -w '%{http_code} %{redirect_url}' \
    -d "csrf=$TENANT_CSRF" -d "plan_code=pro" -d "kind=$kind" "$@" \
    "$BASE/portal/services"
}

# wait_http <url> <label> [attempts]
wait_http() {
  local url="$1" label="$2" attempts="${3:-120}"
  for _ in $(seq 1 "$attempts"); do
    if curl -fsS --max-time 3 "$url" >/dev/null 2>&1; then
      ok "$label answers on $url"
      return 0
    fi
    sleep 1
  done
  bad "$label did not answer on $url"
  return 1
}

# wait_tcp <host> <port> <label> [attempts]
wait_tcp() {
  local host="$1" port="$2" label="$3" attempts="${4:-120}"
  for _ in $(seq 1 "$attempts"); do
    if timeout 2 bash -c "cat < /dev/null > /dev/tcp/$host/$port" 2>/dev/null; then
      ok "$label accepts TCP on $host:$port"
      return 0
    fi
    sleep 1
  done
  bad "$label did not accept TCP on $host:$port"
  return 1
}

# psql_ok <unit> <database> <password> <label>
psql_ok() {
  local unit="$1" db="$2" password="$3" label="$4"
  for _ in $(seq 1 30); do
    if runuser -u t01 -- env XDG_RUNTIME_DIR="/run/user/$U" \
        podman exec "$unit" env PGPASSWORD="$password" \
        psql -U postgres -d "$db" -tAc 'select 1' 2>/dev/null | grep -qx '1'; then
      ok "$label accepts the tenant password"
      return 0
    fi
    sleep 1
  done
  bad "$label rejected the tenant password"
  return 1
}

# 1. Static blog (nginx)
OUT=$(provision web)
echo "$OUT" | grep -q "30[23]" && ok "web template accepted" || bad "web template ($OUT)"
wait_http "http://127.0.0.1:18001/" "blog nginx" || true

# 1b. The tenant writes the site through the panel; nginx must serve it.
WEB_ID=$(curl -fsS -b "$JAR" "$BASE/portal" | grep -o '/portal/services/[0-9]*' | head -1 | grep -o '[0-9]*$')
if [[ -n "$WEB_ID" ]]; then
  SITE=$(curl -fsS -b "$JAR" -o /dev/null -w '%{http_code}' \
    -d "csrf=$TENANT_CSRF" -d "site_path=index.html" \
    --data-urlencode 'site_content=<!doctype html><h1>HoneNode tenant content</h1>' \
    "$BASE/portal/services/$WEB_ID/site")
  echo "$SITE" | grep -q "30[23]" && ok "site content written" || bad "site content write ($SITE)"
  curl -fsS "http://127.0.0.1:18001/" | grep -q "HoneNode tenant content" \
    && ok "nginx serves tenant content" || bad "nginx did not serve tenant content"
  SITE=$(curl -fsS -b "$JAR" -o /dev/null -w '%{http_code}' \
    -d "csrf=$TENANT_CSRF" -d "site_path=css/site.css" \
    --data-urlencode 'site_content=body{color:#0f0}' \
    "$BASE/portal/services/$WEB_ID/site")
  echo "$SITE" | grep -q "30[23]" && ok "site subdirectory written" || bad "site subdirectory write ($SITE)"
  curl -fsS "http://127.0.0.1:18001/css/site.css" | grep -q '#0f0' \
    && ok "nginx serves nested site content" || bad "nginx did not serve nested site content"
else
  bad "could not resolve the web service id"
fi

# 2. Shop database (PostgreSQL)
OUT=$(provision database --data-urlencode "image=$DB_IMAGE" -d "database_password=$DB_PASSWORD_ONE")
echo "$OUT" | grep -q "30[23]" && ok "database template accepted" || bad "database template ($OUT)"
wait_tcp 127.0.0.1 15431 "shop postgres" || true
psql_ok t01-db t01 "$DB_PASSWORD_ONE" "shop postgres"

# 3. Media vault (File Browser)
OUT=$(provision storage -d "file_manager_username=admin" -d "file_manager_password=BenchmarkVault2026")
echo "$OUT" | grep -q "30[23]" && ok "storage template accepted" || bad "storage template ($OUT)"
wait_http "http://127.0.0.1:19001/" "media vault" || true

# 4. Git forge (Forgejo)
OUT=$(provision git)
echo "$OUT" | grep -q "30[23]" && ok "git template accepted" || bad "git template ($OUT)"
wait_http "http://127.0.0.1:20001/" "git forge" || true
wait_tcp 127.0.0.1 22001 "git forge ssh" || true

# 5. A second instance of the same kind gets its own unit, port and data dir.
OUT=$(provision web)
echo "$OUT" | grep -q "30[23]" && ok "second web instance accepted" || bad "second web instance ($OUT)"
wait_http "http://127.0.0.1:18101/" "second nginx" || true
OUT=$(provision database --data-urlencode "image=$DB_IMAGE" -d "database_password=$DB_PASSWORD_TWO")
echo "$OUT" | grep -q "30[23]" && ok "second database instance accepted" || bad "second database instance ($OUT)"
wait_tcp 127.0.0.1 15531 "second postgres" || true
psql_ok t01-db-2 t01 "$DB_PASSWORD_TWO" "second postgres"

# 6. Delete and re-provision the web template: the kind must be free again.
WEB_ID=$(curl -fsS -b "$JAR" "$BASE/portal" | grep -o '/portal/services/[0-9]*' | head -1 | grep -o '[0-9]*$')
if [[ -n "$WEB_ID" ]]; then
  DEL=$(curl -fsS -b "$JAR" -o /dev/null -w '%{http_code}' \
    -d "csrf=$TENANT_CSRF" "$BASE/portal/services/$WEB_ID/delete")
  echo "$DEL" | grep -q "30[23]" && ok "web service deleted" || bad "web service delete ($DEL)"
  OUT=$(provision web)
  echo "$OUT" | grep -q "30[23]" && ok "web re-provisioned after delete" || bad "web re-provision ($OUT)"
  wait_http "http://127.0.0.1:18001/" "blog nginx (re-provisioned)" || true
else
  bad "could not resolve the web service id"
fi

phase "Summary"
echo "PASS=$PASS FAIL=$FAIL"
exit $(( FAIL > 0 ))

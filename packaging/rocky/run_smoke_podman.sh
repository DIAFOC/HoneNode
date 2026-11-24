#!/bin/bash
# =============================================================================
# Run the Rocky smoke from any podman-capable host.
#
# Boots Rocky Linux 10 with systemd as PID 1 (privileged, for nested rootless
# podman), mounts this repository at /workspace, waits for the system to come
# up, and runs smoke.sh inside via podman exec. Equivalent to doing the same
# by hand in a distrobox Rocky box.
#
#   bash packaging/rocky/run_smoke_podman.sh [--app-only|--build-only] [--prebuilt]
#   bash packaging/rocky/run_smoke_podman.sh --bundle <honenode-*.tar.gz>
#
# --bundle installs the OS-tagged bundle with install.py inside the Rocky
# container, verifies the two services, and then runs the smoke against the
# bundle binaries. This is the CI path: a GitHub Actions job container cannot
# boot systemd because the runner overrides its entrypoint, so the Rocky init
# container must be started here, from the host.
# =============================================================================
set -euo pipefail

cd "$(dirname "$0")/../.."
REPO=$(pwd)
IMAGE=${SMOKE_IMAGE:-quay.io/rockylinux/rockylinux:10-ubi-init}
NAME=honenode-rocky-smoke
ARGS=()
PREBUILT=0
BUNDLE_ARCHIVE=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --app-only) ARGS+=(--app-only); shift ;;
    --build-only) ARGS+=(--build-only); shift ;;
    --prebuilt) PREBUILT=1; shift ;;
    --bundle)
      [[ $# -ge 2 ]] || { echo "usage: run_smoke_podman.sh --bundle <honenode-*.tar.gz>" >&2; exit 2; }
      BUNDLE_ARCHIVE=$2
      shift 2
      ;;
    *) echo "usage: run_smoke_podman.sh [--app-only|--build-only] [--prebuilt] [--bundle <archive>]" >&2; exit 2 ;;
  esac
done

BIN_ENV=()
BUNDLE_MOUNT=()
PACKAGES="curl jq sqlite podman passt shadow-utils systemd-udev"
if [[ -n "$BUNDLE_ARCHIVE" ]]; then
  BUNDLE_ARCHIVE=$(realpath "$BUNDLE_ARCHIVE")
  [[ -f "$BUNDLE_ARCHIVE" ]] || { echo "FAIL: bundle archive not found: $BUNDLE_ARCHIVE" >&2; exit 1; }
  BUNDLE_FILE=$(basename "$BUNDLE_ARCHIVE")
  BUNDLE_MOUNT+=(-v "$(dirname "$BUNDLE_ARCHIVE"):/bundle:ro")
  PACKAGES="$PACKAGES python3 policycoreutils libcap util-linux"
elif [[ "$PREBUILT" == 1 ]]; then
  HONED_BIN="$REPO/packaging/os/dist/rocky/honed"
  HONENODE_BIN="$REPO/packaging/os/dist/rocky/honenode"
  if [[ ! -x "$HONED_BIN" || ! -x "$HONENODE_BIN" ]]; then
    echo "FAIL: --prebuilt expects packaging/os/dist/rocky/{honed,honenode}" >&2
    exit 1
  fi
  BIN_ENV+=(-e HONED_BIN=/workspace/packaging/os/dist/rocky/honed)
  BIN_ENV+=(-e HONENODE_BIN=/workspace/packaging/os/dist/rocky/honenode)
else
  PACKAGES="$PACKAGES rust cargo"
fi

podman rm -f "$NAME" >/dev/null 2>&1 || true
# Rootful Podman on a systemd runner needs the host cgroup namespace for the
# nested Rocky systemd; rootless Podman keeps its own namespace.
CGROUP_ARGS=()
if [[ $EUID -eq 0 ]]; then
  CGROUP_ARGS+=(--cgroupns=host)
fi
# No command override: --systemd=always boots systemd as PID 1.
if ! podman run -d --name "$NAME" \
  --hostname rocky-smoke \
  --privileged \
  --systemd=always \
  "${CGROUP_ARGS[@]}" \
  -v "$REPO:/workspace" \
  "${BUNDLE_MOUNT[@]}" \
  -e HONENODE_REPO=/workspace \
  "$IMAGE" >/dev/null; then
  echo "FAIL: podman could not start $IMAGE" >&2
  podman logs "$NAME" 2>&1 | tail -80 >&2 || true
  exit 1
fi

cleanup() { [[ "${SMOKE_KEEP:-0}" == 1 ]] || podman rm -f "$NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "Waiting for systemd inside $IMAGE…"
state=""
for _ in $(seq 1 60); do
  state=$(podman exec "$NAME" systemctl is-system-running 2>/dev/null || true)
  case "$state" in
    running|degraded) break ;;
  esac
  sleep 1
done
if [[ "$state" != "running" && "$state" != "degraded" ]]; then
  echo "FAIL: systemd did not reach running/degraded (state=${state:-unknown})" >&2
  podman logs "$NAME" 2>&1 | tail -80 >&2 || true
  exit 1
fi
podman exec "$NAME" systemctl is-system-running || true

# The smoke needs the Rust toolchain and the container runtime stack; install
# them once here so smoke.sh stays focused on the acceptance flow.
podman exec "$NAME" dnf -y install $PACKAGES

if [[ -n "$BUNDLE_ARCHIVE" ]]; then
  podman exec "$NAME" mkdir -p /tmp/bundle
  podman exec "$NAME" tar -xzf "/bundle/$BUNDLE_FILE" -C /tmp/bundle
  bundle_dir=$(podman exec "$NAME" find /tmp/bundle -maxdepth 1 -mindepth 1 -type d -name 'honenode-*' -print -quit)
  if [[ -z "$bundle_dir" ]]; then
    echo "FAIL: $BUNDLE_FILE did not contain a honenode-* directory" >&2
    exit 1
  fi
  echo "Installing bundle from $bundle_dir ..."
  podman exec "$NAME" python3 "$bundle_dir/install.py" --yes --install-deps
  podman exec "$NAME" systemctl --no-pager --full status honed.service honenode.service
  podman exec "$NAME" curl -fsS http://127.0.0.1:8088/healthz >/dev/null
  BIN_ENV+=(-e HONED_BIN="$bundle_dir/broker/honed")
  BIN_ENV+=(-e HONENODE_BIN="$bundle_dir/native/honenode")
fi

podman exec \
  -e HONENODE_REPO=/workspace \
  -e HONE_SUBID_BASE=2000 \
  -e HONE_SUBID_COUNT=63536 \
  "${BIN_ENV[@]}" \
  "$NAME" \
  bash /workspace/packaging/rocky/smoke.sh ${ARGS[@]+"${ARGS[@]}"}

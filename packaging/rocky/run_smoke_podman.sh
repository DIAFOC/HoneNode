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
# =============================================================================
set -euo pipefail

cd "$(dirname "$0")/../.."
REPO=$(pwd)
IMAGE=${SMOKE_IMAGE:-quay.io/rockylinux/rockylinux:10-ubi-init}
NAME=honenode-rocky-smoke
ARGS=()
PREBUILT=0
for arg in "$@"; do
  case "$arg" in
    --app-only) ARGS+=(--app-only) ;;
    --build-only) ARGS+=(--build-only) ;;
    --prebuilt) PREBUILT=1 ;;
    *) echo "usage: run_smoke_podman.sh [--app-only|--build-only] [--prebuilt]" >&2; exit 2 ;;
  esac
done

BIN_ENV=()
PACKAGES="curl jq sqlite podman passt shadow-utils systemd-udev"
if [[ "$PREBUILT" == 1 ]]; then
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
# No command override: --systemd=always boots systemd as PID 1.
if ! podman run -d --name "$NAME" \
  --hostname rocky-smoke \
  --privileged \
  --systemd=always \
  -v "$REPO:/workspace" \
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

podman exec \
  -e HONENODE_REPO=/workspace \
  -e HONE_SUBID_BASE=2000 \
  -e HONE_SUBID_COUNT=63536 \
  "${BIN_ENV[@]}" \
  "$NAME" \
  bash /workspace/packaging/rocky/smoke.sh ${ARGS[@]+"${ARGS[@]}"}

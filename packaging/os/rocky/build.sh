#!/bin/bash
# Build and verify the Rocky Linux-tagged evaluation bundle.
#
# Rocky 10 is booted in a privileged systemd container (same accommodations as
# the smoke), the binaries are built there with Rocky's toolchain, copied back
# to packaging/os/dist/rocky/, and tagged "os": "rocky" in the manifest.
#
#   bash packaging/os/rocky/build.sh
#
# The container build is deliberately not shared with the Fedora build: a
# Fedora glibc binary can require a newer loader than Rocky provides.
set -euo pipefail

cd "$(dirname "$0")/../../.."
REPO=$(pwd)
# The smoke defaults to /workspace; in a GitHub container job the checkout is
# elsewhere, so point it at this repository explicitly.
export HONENODE_REPO="$REPO"
BUNDLE_VERSION=${BUNDLE_VERSION:-0.8.8}

if [[ "${ID:-}" != "rocky" && "${ID_LIKE:-}" != *"rocky"* ]]; then
  if ! command -v podman >/dev/null; then
    echo "Run this inside Rocky, or install podman to boot the Rocky container." >&2
    exit 1
  fi
  echo "Booting Rocky 10 and building the binaries inside it ..."
  BUILD_ONLY=1 bash packaging/rocky/run_smoke_podman.sh --build-only
else
  echo "Building inside Rocky ..."
  BUILD_ONLY=1 bash packaging/rocky/smoke.sh --build-only
fi

mkdir -p packaging/os/dist
python3 packaging/os/build_os_bundle.py \
  --os rocky \
  --version "$BUNDLE_VERSION" \
  --app-binary packaging/os/dist/rocky/honenode \
  --broker-binary packaging/os/dist/rocky/honed \
  --static-dir backend/static \
  --output "packaging/os/dist/honenode-${BUNDLE_VERSION}-rocky-x86_64.tar.gz"

echo "Rocky bundle ready: packaging/os/dist/honenode-${BUNDLE_VERSION}-rocky-x86_64.tar.gz"
echo "This artifact is Rocky-tagged; do not install it on Fedora."

#!/bin/bash
# Build and verify the Fedora-tagged evaluation bundle.
#
# The binaries are built on Fedora and the manifest records "os": "fedora".
# The same source is used for the Rocky bundle, but the binaries are built on
# Rocky; one tarball is not expected to boot on both.
#
#   bash packaging/os/fedora/build.sh [--skip-tests]
set -euo pipefail

cd "$(dirname "$0")/../../.."
REPO=$(pwd)
BUNDLE_VERSION=${BUNDLE_VERSION:-0.8.8}
SKIP_TESTS=0
for arg in "$@"; do
  case "$arg" in
    --skip-tests) SKIP_TESTS=1 ;;
    *) echo "usage: build.sh [--skip-tests]" >&2; exit 2 ;;
  esac
done

. /etc/os-release
if [[ "${ID:-}" != "fedora" ]]; then
  echo "This bundle must be built on Fedora (got ID=${ID:-unknown})." >&2
  echo "Use packaging/os/rocky/build.sh inside Rocky for the Rocky bundle." >&2
  exit 1
fi

cargo build --release --manifest-path backend/Cargo.toml -p honed -p honenode

if [[ "$SKIP_TESTS" == 0 ]]; then
  cargo test --manifest-path backend/Cargo.toml
  python3.14 -m unittest discover -s backend/crates/honenode/tests
  python3.14 -m unittest discover -s packaging/tests
fi

mkdir -p packaging/os/dist
python3 packaging/os/build_os_bundle.py \
  --os fedora \
  --version "$BUNDLE_VERSION" \
  --app-binary backend/target/release/honenode \
  --broker-binary backend/target/release/honed \
  --static-dir backend/static \
  --output "packaging/os/dist/honenode-${BUNDLE_VERSION}-fedora-x86_64.tar.gz"

echo "Fedora bundle ready: packaging/os/dist/honenode-${BUNDLE_VERSION}-fedora-x86_64.tar.gz"
echo "This artifact is Fedora-tagged; do not install it on Rocky."

#!/bin/sh
# HoneNode bundle entry point. Installs python3 if needed, then runs install.py.
set -eu
cd "$(dirname "$0")"
if ! command -v python3 >/dev/null 2>&1; then
  if command -v dnf >/dev/null 2>&1; then
    dnf install -y python3
  else
    echo "error: python3 is required and dnf is not available" >&2
    exit 1
  fi
fi
exec python3 install.py "$@"

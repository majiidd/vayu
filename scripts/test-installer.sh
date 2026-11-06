#!/bin/sh
# Exercise the release installer against a real archive and a loopback HTTP server.
# Usage: scripts/test-installer.sh [DIST]
# Needs Python 3; uses the host's /bin/sh (including BusyBox sh in Alpine).
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
exec python3 "$root/scripts/test-installer.py" "${1:-dist}"

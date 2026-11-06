#!/bin/sh
# Install vayu from a directory of release archives with scripts/install.sh, under BusyBox sh.
#
# Usage: scripts/test-installer.sh [DIST]
#
# DIST (default: dist) holds the archive for this machine's architecture and SHA256SUMS. The
# directory is served on 127.0.0.1 and the installer runs in an Alpine container on the host
# network. Needs python3 and Docker.
set -eu

dist=${1:-dist}
port=${PORT:-8765}
root=$(cd "$(dirname "$0")/.." && pwd)

[ -f "$dist/SHA256SUMS" ] || {
  echo "test-installer: $dist/SHA256SUMS not found; run make checksums" >&2
  exit 1
}

python3 -m http.server "$port" --bind 127.0.0.1 --directory "$dist" >/dev/null 2>&1 &
server=$!
trap 'kill "$server" 2>/dev/null || true' EXIT
sleep 1

docker run --rm --network host -e VAYU_BASE_URL="http://127.0.0.1:$port" \
  -v "$root/scripts/install.sh:/install.sh:ro" alpine:latest \
  sh -c 'sh /install.sh && ~/.local/bin/vayu --version'

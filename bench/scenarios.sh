#!/usr/bin/env bash
# Run bench/compare.sh against bench/server.py under synthetic server conditions on this machine.
#
# Usage: bench/scenarios.sh [REPETITIONS] [SCENARIO...]
#
# The conditions come from the server (per-connection limits, a shared budget, first-byte
# latency) on loopback, not from a real network: they show how vayu's segmentation and
# concurrency controller respond to each kind of limit, not how fast any real link is. Build
# first with `cargo build --release`.
set -euo pipefail

cd "$(dirname "$0")"
reps=${1:-3}
shift || true

# Server arguments for each scenario (bash 3.2 compatible: no associative arrays).
conditions() {
  case $1 in
    clean) echo "--size 256" ;;
    per-conn-2M) echo "--size 24 --per-conn 2M" ;;
    per-conn-1M-250ms) echo "--size 12 --per-conn 1M --latency 250" ;;
    shared-8M) echo "--size 64 --global 8M" ;;
    per-conn-1M-shared-4M) echo "--size 16 --per-conn 1M --global 4M" ;;
    no-ranges-4M) echo "--size 32 --per-conn 4M --no-ranges" ;;
    *) echo "unknown scenario $1" >&2; exit 2 ;;
  esac
}
order=(clean per-conn-2M per-conn-1M-250ms shared-8M per-conn-1M-shared-4M no-ranges-4M)
(($#)) && order=("$@")

for name in "${order[@]}"; do
  args=$(conditions "$name")
  log=$(mktemp)
  # shellcheck disable=SC2086
  python3 -u server.py $args >"$log" &
  server=$!
  trap 'kill $server 2>/dev/null || true' EXIT
  until [[ -s $log ]]; do sleep 0.1; done
  base=$(head -1 "$log")
  echo "== $name ($args)"
  STATS_URL="$base/__stats" ./compare.sh "$base/file.bin" "$reps"
  kill "$server"
  wait "$server" 2>/dev/null || true
  rm -f "$log"
  echo
done

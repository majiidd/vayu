#!/usr/bin/env bash
# Baseline comparison of vayu against curl and aria2 on one URL.
#
# Usage: bench/compare.sh URL [REPETITIONS]
#
# Each run downloads into a fresh temporary directory; tools run in a shuffled order every round
# to spread out network drift. Results are wall-clock completion times including connection setup
# and finalization, plus peak resident memory. `vayu-1` is vayu limited to one connection, the
# baseline for its adaptive segmentation. Record the network conditions (bandwidth, latency,
# loss, route, proxy) next to every published result. To impose controlled conditions on Linux,
# use netem on the client interface, e.g.:
#
#   sudo tc qdisc add dev eth0 root netem delay 250ms loss 2% rate 2mbit
#   sudo tc qdisc del dev eth0 root
#
# With STATS_URL pointing at bench/server.py's /__stats endpoint, each row also shows the
# requests the tool made, the bytes the server sent (more than the file size means wasted
# transfer), and the most connections it had open at once.
set -euo pipefail

url=${1:?usage: bench/compare.sh URL [REPETITIONS]}
reps=${2:-5}
vayu=${VAYU:-$(dirname "$0")/../target/release/vayu}
stats=${STATS_URL:-}

tools=(vayu vayu-1 curl curl-retry)
command -v aria2c >/dev/null && tools+=(aria2-default aria2-x8)

if [[ $(uname) == Darwin ]]; then
  time_cmd=(/usr/bin/time -l)
  rss_field='maximum resident set size'
  rss_scale=1024 # bytes -> KiB
else
  time_cmd=(/usr/bin/time -v)
  rss_field='Maximum resident set size'
  rss_scale=1
fi

now() { python3 -c 'import time; print(time.time())'; }

run() {
  local tool=$1 dir=$2
  case $tool in
    vayu) "$vayu" "$url" -o "$dir/out" --ui off ;;
    vayu-1) "$vayu" "$url" -o "$dir/out" --ui off --max-connections 1 ;;
    curl) curl -sSfL -o "$dir/out" "$url" ;;
    curl-retry) curl -sSfL --retry 10 --retry-all-errors -C - -o "$dir/out" "$url" ;;
    aria2-default) aria2c -q -d "$dir" -o out "$url" ;;
    aria2-x8) aria2c -q -x8 -s8 -k1M -d "$dir" -o out "$url" ;;
  esac
}

printf '%-14s %10s %12s %-16s %s\n' tool seconds peak_rss_kib sha256 server
for ((round = 1; round <= reps; round++)); do
  for tool in $(printf '%s\n' "${tools[@]}" | awk 'BEGIN{srand()} {print rand() "\t" $0}' | sort | cut -f2); do
    dir=$(mktemp -d)
    [[ -n $stats ]] && curl -sf "${stats%/__stats}/__reset" >/dev/null
    start=$(now)
    if ! "${time_cmd[@]}" bash -c "$(declare -f run); url='$url' vayu='$vayu' run $tool '$dir'" 2>"$dir/time.log"; then
      printf '%-14s %10s\n' "$tool" FAILED
      rm -rf "$dir"
      continue
    fi
    end=$(now)
    rss=$(awk -v f="$rss_field" -v s="$rss_scale" 'index($0, f) { gsub(/[^0-9]/, "", $0); print int($0 / s) }' "$dir/time.log")
    digest=$(shasum -a 256 "$dir/out" | cut -c1-16)
    server=''
    [[ -n $stats ]] && server=$(curl -sf "$stats" | python3 -c 'import json, sys; print("requests=%(requests)s sent=%(bytes_sent)s peak_conns=%(peak_connections)s" % json.load(sys.stdin))')
    printf '%-14s %10.2f %12s %-16s %s\n' "$tool" "$(echo "$end - $start" | bc)" "${rss:-?}" "$digest" "$server"
    rm -rf "$dir"
  done
done

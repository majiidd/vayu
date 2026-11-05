# Benchmarks

These results show how vayu's segmentation and concurrency controller respond to different kinds
of server-side limits. They come from a synthetic server on loopback, not from a real network.
They are evidence for the defaults in [ARCHITECTURE.md](ARCHITECTURE.md#segmented-transfers).
They are not a claim about how fast vayu is on any real link.

## Harness

- [`bench/server.py`](../bench/server.py) serves a deterministic payload.
  - It sends a strong ETag and honors `Range` and `If-Range`.
  - It can limit each response (`--per-conn`), share one budget across all responses
    (`--global`), and delay response headers (`--latency`).
  - `/__stats` reports the requests made, bytes sent and peak responses in flight since the last
    `/__reset`.
- [`bench/compare.sh`](../bench/compare.sh) downloads one URL with each tool into a fresh
  directory. It shuffles the tool order every round and records:
  - wall-clock time;
  - peak RSS;
  - the first 16 hex digits of the output's SHA-256;
  - the server's counters.
- [`bench/scenarios.sh`](../bench/scenarios.sh) starts the server under each condition below and
  runs the comparison.

To reproduce:

```sh
cargo build --release
bench/scenarios.sh 3 | tee bench.txt  # every scenario, three rounds
bench/summarize.py bench.txt          # the median table below; fails if any output differed
bench/scenarios.sh 5 per-conn-2M      # one scenario, five rounds
```

Every release candidate is checked against this table (see [RELEASING.md](RELEASING.md)).

The tools compared:

| Tool | Command |
|---|---|
| `vayu` | `vayu URL -o out --ui off` (defaults: up to 8 connections, adaptive) |
| `vayu-1` | the same with `--max-connections 1`, the single-stream baseline |
| `curl` | `curl -sSfL -o out URL` |
| `curl-retry` | `curl -sSfL --retry 10 --retry-all-errors -C - -o out URL` |

aria2 was not installed on the measurement machine, so it is not in these tables.
`compare.sh` adds `aria2-default` and `aria2-x8` automatically when `aria2c` is on `PATH`.

## Results

- **Run:** 2026-11-05, vayu 0.1.0 (release profile).
- **Machine:** Apple M4 Pro, 24 GiB, macOS 27.0.1.
- **Server:** Python 3.15.2 over IPv6 loopback.
- **Tools:** curl 8.7.1.
- **Method:** three rounds per scenario; tables show the median.
- **Correctness:** every run of every tool produced the same digest as the others in its
  scenario.
- **Server columns:** from vayu's median run.
  - **sent − size** is the number of bytes that were transferred more than once.

| Scenario | Size | vayu | vayu-1 | curl | Requests | Peak connections | Sent − size |
|---|---:|---:|---:|---:|---:|---:|---:|
| `clean`: no limits | 256 MiB | 0.57 s | 0.57 s | 0.17 s | 1 | 1 | 0 |
| `per-conn-2M`: 2 MiB/s per response | 24 MiB | **7.40 s** | 12.46 s | 12.06 s | 3 | 3 | 64 KiB |
| `per-conn-1M-250ms`: 1 MiB/s per response, 250 ms to headers | 12 MiB | **8.67 s** | 12.73 s | 12.32 s | 2 | 2 | 64 KiB |
| `shared-8M`: one 8 MiB/s budget | 64 MiB | 7.91 s | 8.17 s | 7.81 s | 3 | 2 | 192 KiB |
| `per-conn-1M-shared-4M`: 1 MiB/s per response, 4 MiB/s total | 16 MiB | **8.93 s** | 16.48 s | 16.07 s | 3 | 3 | 64 KiB |
| `no-ranges-4M`: 4 MiB/s, `Accept-Ranges: none` | 32 MiB | 8.42 s | 8.41 s | 8.05 s | 1 | 1 | 0 |

The spread between rounds was small:
- vayu ranged 7.40–7.66 s in `per-conn-2M`, 8.66–8.68 s in `per-conn-1M-250ms` and
  8.93–9.26 s in `per-conn-1M-shared-4M`.
- The widest spread was in `shared-8M`, where all tools varied by about ±0.3 s.

## What the results support

- **Per-connection limits:** extra connections pay off.
  - `per-conn-2M`: 1.7× faster than one stream.
  - `per-conn-1M-250ms`: 1.5× faster.
  - `per-conn-1M-shared-4M`: 1.8× faster.
  - The controller added connections only while each one raised throughput by at least 10%.
- **Shared limit:** when the limit is shared, extra connections don't help, and the controller
  does not keep them.
  - In `shared-8M` it probed a second connection, measured no gain, and never went past two.
  - vayu's time is within the round-to-round noise of curl and vayu-1.
  - The probe cost 192 KiB of repeated transfer, 0.3% of the file.
- **No range support:** the server advertises `Accept-Ranges: none`, so vayu never sends a range
  request and behaves exactly like the single-stream baseline.
- **Wasted transfer:** stays within one to three 64 KiB blocks per run.
  - The server counts bytes it *wrote*, so this includes data that was still in flight on a
    connection vayu ended at a segment boundary.
  - That data is discarded rather than written twice. This column measures repeated network
    transfer, not damage to the file.
- **Ramp-up is conservative by design**, so the gains are smaller than the ideal.
  - The ideal for `per-conn-1M-shared-4M` is about 4 s with four connections.
  - Each step waits for a warmup and a measurement window, and on small files most of the
    download is over before the ramp completes.
  - This trades peak speed on short downloads for not overloading servers and links that are
    already saturated.

## Known gaps

- **Unthrottled loopback:** vayu is about 3× slower than curl here (about 450 MiB/s against
  1.5 GiB/s), and its peak RSS is about 43 MiB against curl's 3 MiB.
  - vayu verifies while it writes: it hashes every 64 KiB block for recovery, keeps a
    whole-file SHA-256, and writes through a bounded queue with periodic durable checkpoints.
  - curl does none of these.
  - This overhead matters only on links faster than a few hundred MiB/s.
- **Not modeled:** packet loss, TCP slow start, bufferbloat, proxies, TLS and HTTP/2.
  - The latency option delays response headers only.
  - Use `netem` on Linux (see the comment at the top of `bench/compare.sh`) or test against
    real servers.
  - Results on real networks should record the bandwidth, latency, loss, route and proxy next
    to every number.
- **No aria2 comparison yet.**

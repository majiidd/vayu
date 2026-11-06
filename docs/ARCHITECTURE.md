# Architecture

Vayu is a single binary crate (`vayu`) with separate internal modules. Modules
will be split into their own crates only when there is real reuse.

```text
main ─ app ─┬─ cli           arguments, TOML config, precedence, exit codes (error.rs)
            ├─ transport     one reqwest client per connection slot, TLS trust, proxies,
            │                cookies, headers, redirects, failure classification, HTTP header
            │                parsing (http.rs)
            ├─ scheduler     Stop handle, retry budget/backoff (retry), the lead stream and
            │                segmentation decision (engine), byte-range ownership (plan),
            │                helper connections (crew), adaptive concurrency (controller)
            ├─ storage       destination choice, locked .part file, writer thread with
            │                positional writes and checkpoints, block ledger, crash-aware
            │                publication
            ├─ state         versioned .vayu format, bounds checks, atomic publication
            ├─ recovery      finding saved progress, verification, continuity decisions,
            │                completing interrupted finalization
            ├─ integrity     expected checksum parsing and comparison
            ├─ metrics       atomic counters → sampler → watch<Snapshot>; bounded event queue
            ├─ presentation  capability detection, palette/glyphs, dashboard, lines, JSON
            └─ fault         named crash-injection points (debug builds only)
```

## Data flow

1. `app` resolves `Settings`, detects terminal capabilities on **stderr**, builds the
   `Transport`, and starts the presenter thread, the signal watcher, and the metrics sampler.
2. `recovery::locate` looks for saved progress before any request is sent. It checks the state
   file named by `--resume-from`, or the one next to the destination implied by `-o` or the URL.
   See [Recovery](#recovery).
3. `scheduler::engine` sends a plain GET through `Transport::get` on connection slot 0. That
   function follows redirects itself, so it can scope custom headers to the original origin and
   reject unsupported schemes.
4. The first usable response decides the destination. If the server names a file that has saved
   progress (`Content-Disposition`), the engine drops that response and continues from the saved
   state with a range request. Otherwise `storage` creates and locks `NAME.part`, and the body
   streams into a `PartWriter`.
5. `PartWriter` is a dedicated OS thread fed by a bounded channel of 64 chunks, shared by all
   connections through `WriteHandle`s. Each chunk carries its offset; the thread coalesces
   consecutive chunks into runs of up to 256 KiB (1 MiB buffered in total) and writes them with
   `pwrite`. It keeps a `Ledger`: one SHA-256 per block plus a whole-file SHA-256 that trails
   the contiguous prefix (see [Segmented transfers](#segmented-transfers)). A slow disk
   therefore slows the network down instead of filling memory, and disk, hash, and checkpoint
   work never runs on the async workers.
6. If the transfer qualifies, the engine turns segmented after this first response; see
   [Segmented transfers](#segmented-transfers).
7. When a retryable failure happens, the engine checkpoints, then continues with `Range` + `If-Range`, or with
   `Range` alone if `--checksum` gates publication. It accepts a 206 only if its `Content-Range`,
   total size, and validators match what came before. A 200 with the same validators means the
   server ignored the range, so the engine starts over. Validators that disagree mean the source
   changed: the engine stops with a conflict and keeps the data.
   Data from an earlier run is never thrown away automatically: if continuing it would require
   starting over, the engine stops with a conflict instead.
8. On completion, the engine stops its helper connections and waits until their last writes
   are queued. `app` then finishes the writer, which publishes a checkpoint recording the
   complete size and returns the length of the contiguous data and its SHA-256. `app` checks
   that this length is the full size and verifies the checksum. Then `Destination::publish` hard-links
   `.part` to the final name. That operation fails if the name already exists. On filesystems
   without hard links, it checks for an existing file and then renames. Afterwards it syncs the
   directory and removes `.part` and `.vayu`.

## State file and checkpoints

`NAME.vayu` is one header line, `vayu-state 1 <SHA-256 of body>`, followed by a JSON body: a
download id, the generation, the file name, a request identity (a SHA-256 of the URL), the
URL's origin and last path segment, the total size, the validators, an optional checksum, the block size, the saved
ranges, and the per-block hashes.

The parser enforces limits before trusting any of it:

- at most 8 MiB of input;
- at most 65,536 blocks and ranges;
- block sizes from 64 KiB to 1 GiB;
- ranges sorted, non-overlapping, and within the total;
- a block count consistent with the ranges;
- hashes only for blocks whose first byte is saved.

New downloads use about 4,096 blocks of at least 1 MiB (16 MiB when the size is unknown).
Every range starts at a block boundary, and every block is written in order from its first
byte, so a block's hash always covers its saved bytes from the block's start. A single-stream
download saves one range from offset zero; a segmented one usually saves several. The format
version did not change for segmented transfers: version 1 always allowed several ranges, and
states written by earlier releases remain valid.

The last block's hash covers only the bytes saved so far, so checkpoints are byte-granular. On a
0.5 Mbit/s link, a crash loses at most the data received since the last checkpoint, not a whole
block.

The writer thread checkpoints in this order:

1. flush the buffer and `sync_data` the part file;
2. write the new generation to `NAME.vayu.tmp` and `sync_all` it;
3. rename it over `NAME.vayu`;
4. sync the directory;
5. report the saved byte count to `metrics::Durable`, which JSON events report as
   `saved_bytes`.

The first generation is published when `.part` is created, before any body bytes. A `.part`
without a `.vayu` therefore always means a foreign or pre-M2 leftover. Resetting to zero
publishes an empty generation before truncating. Fault points `checkpoint-synced`,
`checkpoint-tmp-written`, `checkpoint-published`, `finalize-before-link`, `finalize-linked`,
and `finalize-part-removed` let the tests abort at each step.

## Recovery

`recovery::examine` inspects one destination:

1. Lock `.part` if it exists, so a running download is never disturbed.
2. Load and validate `.vayu`. A damaged or unsupported file is a conflict, and nothing is
   modified.
3. Check that the stored file name matches. All paths derive from the state file's location, so
   a moved directory still resumes.
4. Match the request identity unless `--resume-from` selected the file. For a destination
   guessed from the URL, a mismatch is deferred, because the server may name another file.
5. If the destination already exists, it counts as finished only if the state is complete and
   the file's size and every block hash match. In that case, remove the leftovers. Otherwise it
   is someone else's file.
6. Re-read every saved range block by block. In each range, keep the longest run of blocks
   whose hashes match; claims beyond the end of the file count as missing. While the verified
   data is contiguous from offset zero, this also rebuilds the whole-file SHA-256.
7. Without a strong validator or a checksum, progress that is not yet complete cannot be
   continued: report a conflict. Otherwise truncate the file after its last verified byte and
   hand the locked file, ledger, and updated state to the engine. Saved progress with gaps is
   always continued in segments, even with `--max-connections 1`, so only the gaps are
   requested. Complete progress goes straight to finalization.

A response with other validators but the same size, as from a mirror with its own copy, is not
a conflict by itself. The engine asks the writer for up to 8 complete saved blocks spread over
the data, requests each again through the original URL with the new validator as `If-Range`,
and compares hashes. Every sample must come back as `206` for exactly that block, under the new
validators; anything else, such as a redirect back to the first copy, is retried without a
verdict. If all hashes match, the new validators replace the old ones in memory, in the crew,
and in a newly published state generation. A single mismatch is a conflict. Helpers that meet
such a copy end as failed and leave the check to the lead.


## Segmented transfers

The engine segments a transfer when the size is known, every range can be bound to the same
representation (a strong validator for `If-Range`, or a `--checksum` that gates publication),
the response is not content-encoded, and either at least four blocks remain with
`--max-connections` above 1, or recovered progress has gaps. The lead's first response then
becomes the first segment, `[0, size)`.

**Plan** (`scheduler::plan`). Every byte is either received, owned by exactly one segment
(between its cursor and its end), or missing. A worker *reserves* bytes under the plan's lock
before it hands them to the writer, so the plan never gives away bytes already in flight.
Segment ids are never reused: a worker whose segment was released cannot reserve anything
more, so late data from an abandoned request cannot land in a reassigned range. Work is taken
in this order:

1. the lowest missing range;
2. otherwise the back half of the segment with the most remaining work, split at a block
   boundary, if both halves keep at least a block and the owner would not finish within a few
   seconds anyway (scaled by the measured time to first byte).

A split is only *proposed* until the thief's response has been validated (206, the requested
start, the same size and validators, identity encoding). Only then does `commit` shrink the
owner's end, and only if the owner has not passed the split point; otherwise the response is
dropped. A server that ignores ranges therefore never costs the first stream its work. When a
segment ends early, its unreceived bytes go back to the segment just before them if that
segment's response still covers them (the open-ended first stream), or back to missing.

**Crew** (`scheduler::crew`). The lead (slot 0) runs in the engine with the retry policy for
the whole transfer. A supervisor task ticks every 250 ms, feeds the controller, starts helper
tasks when it wants more connections and there is work, and retires the newest helpers when it
wants fewer: a retired helper stops at its next block boundary and returns the rest. Helpers
request closed ranges, so a connection whose range ends normally is reused for the next one.
They never retry on their own. On a failure a helper returns its range and reports how it ended:

| Helper outcome | Effect |
| --- | --- |
| Validators or size differ (206 or 200) | The transfer stops with a conflict; data is kept |
| 200 to a range request, 416, wrong `Content-Range`, compressed partial response | Segmentation ends (one notice); the lead continues |
| 4xx before any range was served | Same as above |
| 4xx after ranges were served, 429, 503 | Treated as throttling: the connection count halves |
| Network error, stall, early close without progress | One connection fewer |
| Write failure | The transfer stops with the writer's error |

The lead's own failures also feed the controller. Any worker's progress resets the shared
retry budget. `--limit-rate` is one limiter shared by all connections.

**Controller** (`scheduler::controller`). A pure state machine over time and the plan's byte
count, unit-tested against simulated links (per-connection limits, a shared limit, saturation,
high latency). It starts at `--min-connections` (one by default) and adds one after a 1 s
warm-up and a 1.5 s baseline. It judges the new connection once it has delivered its first byte, ramped up for
max(0.5 s, 3 × TTFB), and been measured for max(1.5 s, 6 × TTFB). It keeps the connection only
when combined throughput rose by at least 10%. Otherwise it retires the connection and holds
growth for a cooldown that starts at 15 s and doubles, up to 2 min, after each fruitless probe.
It does not grow when less than max(2 s, 8 × TTFB) of transfer remains at the current rate, or
while the writer's queue is nearly full. A level whose throughput stays 20% below what one
connection fewer achieved, over 8 s, is abandoned. None of these throughput decisions go below
`--min-connections`. Throttling and failures may, since the server asked for fewer; once the
cooldown passes, the controller returns to the minimum directly, without a probe.

**Connection topology.** `Transport` builds one reqwest client per slot. They share the
cookie jar and the certificate roots loaded at startup. Each client keeps at most one idle
connection per host, so slot *n* reuses its own connection, and *N* workers use *N* transport
connections. A single shared client would multiplex HTTP/2 requests onto one connection,
where they share one congestion window and any per-connection limit.

**Whole-file digest.** Data that arrives exactly where the running hash stands (the lead, in
the common case) is hashed in memory. When another connection's data becomes contiguous with
the prefix, the writer reads it back with `pread` at the next checkpoint (usually from the page
cache) and hashes it. Each byte is hashed once for the digest.

## Concurrency and isolation

- Tokio multi-thread runtime with two workers.
- Every connection records bytes in a shared `AtomicU64` after the writer accepts them. A
  sampler task publishes the counter as `Snapshot`s on a `watch` channel every 125 ms. Every
  other tick (250 ms) it also updates the speed and ETA windows. The finer progress cadence
  lets a local dashboard reach its 8 frames per second. Connections that are receiving hold a `StreamGuard`; the
  count is the dashboard's "Connections", and while any stream is active the phase shows
  as downloading even if the lead is reconnecting. Discrete events go
  through a bounded `sync_channel` with `try_send`; events are dropped rather than ever blocking
  the transfer.
- The presenter runs on its own OS thread. It reads the latest snapshot at the frame rate and
  writes each frame in one batch. If a write fails, it disables that output and lets the
  download continue. The one exception is `--json`: when stdout breaks, the presenter requests
  a stop and Vayu exits with 141, because the consumer is gone.
- `Stop` is a first-reason-wins `watch` flag. Every await point in the engine selects on it.
  After a stop, the engine finishes the writer, so data already received is synced and
  checkpointed before exit. This has a 10-second limit, so a stalled disk cannot hold the
  process indefinitely.

## Presentation decisions

Layout, color, glyphs, and animation are resolved independently in `presentation::caps`:

- Cursor control requires stderr to be a TTY and `TERM` to be set to something other than
  `dumb`.
- Color follows the order explicit option → `NO_COLOR` → capability.
- Unicode glyphs require a UTF-8 locale, and are turned off on `TERM=linux`.
- Animation requires `--no-animation` to be unset and cursor control to be available.

The dashboard diffs each frame against the previous one line by line and clips every line to
the terminal width minus one, so lines never wrap. When the width changes, it counts how the old
frame rewrapped and erases those rows before redrawing.

Styles are written as plain SGR sequences:

- `3X` for the 16-color baseline;
- `38;5;N` and `38;2;R;G;B` only at those depths.

So a 16-color terminal never receives an extended color code.

Before each frame, the dashboard compares the terminal's foreground process group
(`tcgetpgrp`) with its own:

- **In the background** (`&`, or Ctrl+Z then `bg`), it skips the frame and forgets the
  previous one. Back in the foreground, the next frame starts below whatever the shell printed
  in the meantime.
- **Under `stty tostop`,** a background write would stop the whole process with `SIGTTOU`. A
  finished background job therefore skips its success summary; a failure is still written.

`tests/terminal.rs` checks these decisions on real pseudo-terminals. The test spawns the binary
with stderr on a PTY slave. Depending on the case, it makes that PTY the controlling terminal
(to test hang-ups), runs under `nohup`, or runs as a job of a job-control shell.

## Release engineering

- `scripts/package.sh` builds `vayu-TARGET.tar.gz` and its checksum.
  - Asset names carry no version, so `releases/latest/download/…` always resolves.
  - `scripts/install.sh` depends on that. It is POSIX `sh`, tested on macOS, Linux, and BusyBox. It
    verifies `SHA256SUMS` and refuses to replace a `vayu` whose `--version` lacks
    `(vayu)`.
- `.github/workflows/ci.yml` runs on branch pushes, pull requests, and manual runs. It is also
  a reusable workflow called by `.github/workflows/release.yml` for version tags.
  - **Gates:** the complete debug test matrix, release script tests, and cargo-deny.
  - **Builds:** six targets, each with a linkage check; Linux targets also run distribution
    smoke tests. The native installer suite runs on macOS and musl Linux; the latter also
    runs it under BusyBox `sh`. The combined release assets are verified and tested again.
  - **macOS:** the binaries carry the linker's ad-hoc signature, and there is no Developer ID
    signing or notarization. The script installer downloads without browser quarantine flags.
- `.github/workflows/release.yml` waits for every shared CI job, attests the tested archives,
  and opens a draft GitHub release. A maintainer publishes it after reviewing the assets and
  notes, as described in `docs/RELEASING.md`.
  - A final tag must match the version in `Cargo.toml` exactly, so its executable reports the
    same version as the release.
  - Publishing a stable release makes its installer and binary archives available through
    `releases/latest/download/…`.

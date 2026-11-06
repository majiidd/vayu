# Vayu — وایو

Vayu is a command-line downloader built for slow, unstable, and high-latency networks. It aims to
finish downloads correctly and keep progress through interruptions, with a clear, colorful
terminal interface that also works over SSH and without a terminal.

```sh
vayu "https://example.com/file.zip"
```

> **Status: release tooling for the public beta (milestone 6) is in place; the first release
> candidate has not been cut yet.** Vayu starts every download on one connection and adds
> more only while measurement shows they raise throughput, for example when the server limits
> each connection. Progress is saved to disk every few seconds. If the connection drops, the
> process is killed, or the machine restarts, running the same command again continues from
> the last checkpoint, but only when Vayu can prove the data belongs to the same file. The
> command line, JSON events, and state format may still change before 1.0; the
> [changelog](CHANGELOG.md) lists every change. See [Roadmap](#roadmap).

## Install

Vayu is a single executable. It needs no Rust toolchain, shared OpenSSL, daemon, or
administrator rights. No release has been published yet; until then, build from source (below).
Each release will provide:

- macOS archives for Apple Silicon (macOS 11+) and Intel (macOS 10.14+);
- static Linux archives for x86-64 and ARM64 that run on any distribution;
- glibc Linux archives.

[docs/COMPATIBILITY.md](docs/COMPATIBILITY.md) lists exact minimums and what is tested.

**Homebrew** (macOS and Linux):

```sh
brew install majiidd/vayu/vayu
```

**Installer** (macOS and Linux). It installs into `~/.local/bin` and checks the release's
`SHA256SUMS` before installing anything. It stops if a different program named `vayu` is
already installed.

```sh
curl -fsSL https://github.com/majiidd/vayu/releases/latest/download/install.sh | sh
curl -fsSL https://github.com/majiidd/vayu/releases/latest/download/install.sh | sh -s -- --version 0.1.0 --dir ~/bin
```

**Manual install.** Download `vayu-TARGET.tar.gz` and `SHA256SUMS` from the release, then:

```sh
sha256sum -c SHA256SUMS --ignore-missing      # shasum -a 256 -c SHA256SUMS on macOS
tar -xzf vayu-aarch64-apple-darwin.tar.gz
mv vayu-aarch64-apple-darwin/vayu ~/.local/bin/
```

Each archive also has a signed build provenance attestation:
`gh attestation verify vayu-TARGET.tar.gz --repo majiidd/vayu`. The macOS binaries are not
signed with an Apple Developer ID. Homebrew and the installer are not affected; after a
browser download, run `xattr -d com.apple.quarantine vayu` once.

**From source**, with stable Rust (`rust-toolchain.toml` pins the tested toolchain):

```sh
cargo install vayu --locked     # from crates.io
cargo install --path . --locked # from a checkout
```

`vayu --version` prints `vayu X.Y.Z (vayu)`.

## Usage

```sh
vayu URL [OPTIONS]

vayu "https://example.com/file.zip" -o ~/Downloads/file.zip
vayu "https://example.com/file.zip" -o ~/Downloads/          # keep the server's file name
vayu "https://example.com/file.zip" --checksum sha256:EXPECTED_DIGEST
vayu "https://example.com/file.zip" --proxy socks5h://127.0.0.1:1080
vayu "https://example.com/file.zip" --header-file request-headers.txt --cookie-file cookies.txt
vayu "https://example.com/file.zip" --ui compact --color auto
vayu "https://example.com/file.zip" --json
vayu "https://example.com/file.zip" --restart
vayu "https://example.com/new-signed-url" --resume-from file.zip.vayu
```

| Option | Behavior |
| --- | --- |
| `-o, --output PATH` | Destination file, or an existing directory (or a path ending in `/`) to save into |
| `--max-connections N` | Upper bound on simultaneous connections (1–64, default 15, or the minimum when that is higher). Vayu adds connections up to this bound only while they help; `1` downloads over a single stream |
| `--min-connections N` | Lower bound on simultaneous connections once a download is segmented (1–64, default 1). Vayu starts with this many and keeps them even when measurement says more do not help. When no maximum is set, it raises the maximum to match. Above an explicit `--max-connections` it is an error on the command line; a configured value is lowered to the maximum |
| `--proxy URL` | `http://`, `https://`, `socks5://` (local DNS) or `socks5h://` (DNS at the proxy); credentials go in the URL |
| `--no-proxy` | Ignore proxy environment variables and system proxy settings |
| `-H, --header NAME:VALUE` | Extra request header (repeatable) |
| `--header-file PATH` | `Name: value` per line; `#` comments. Keeps secrets out of the process list |
| `--cookie-file PATH` | Netscape/Mozilla `cookies.txt` (the format browser export extensions produce) |
| `--ca-cert PATH` | Also trust these PEM certificates (repeatable). Verification is never disabled |
| `--checksum sha256:HEX` | Publish the file only if its SHA-256 matches |
| `--restart` | Discard the saved progress (`.part` and `.vayu`) for this destination and start over. Never overwrites a completed file |
| `--resume-from PATH` | Continue the download saved in `PATH` (`NAME.vayu`, or `NAME` next to it), for example with a fresh signed URL |
| `--retry auto\|N\|forever` | `auto` keeps retrying until 10 minutes pass with no useful progress. `N` gives up after N consecutive attempts that made no progress |
| `--connect-timeout DURATION` | Limit for establishing a connection (default `30s`) |
| `--stall-timeout DURATION` | Reconnect after this long without data (default `60s`) |
| `--limit-rate RATE` | Bandwidth ceiling: `500K`, `2M`, `1.5MiB` (binary units). `kB`/`MB` are decimal |
| `--http-version auto\|1.1\|2` | Force a protocol for troubleshooting or comparisons |
| `--ui auto\|compact\|detailed\|off` | Presentation mode |
| `--color auto\|always\|never` | Color, independent of layout |
| `--theme auto\|dark\|light` | Palette; `auto` uses `COLORFGBG` when available, otherwise your terminal's own colors |
| `--glyphs auto\|unicode\|ascii` | Symbols, independent of color |
| `--no-animation` | No spinner |
| `-q, --quiet` | Print only actionable failures |
| `--json` | JSON Lines events on stdout (see below) |

Durations take forms such as `30s`, `500ms`, `5m`, `1h`, or `1m30s`.

## How Vayu keeps data correct

- Data goes to `NAME.part` next to the destination, and progress is saved in `NAME.vayu`
  (see [Saved progress and resuming](#saved-progress-and-resuming)). Only complete data reaches
  the final name: Vayu checks the byte count, verifies the checksum when one is given, syncs the
  data to disk, and then links it into place with an operation that never replaces an existing
  file.
- An existing destination is never overwritten. Saved progress is reused only when it matches
  the request; otherwise Vayu stops and explains. Two Vayu processes cannot write the same
  `.part` file; it is locked.
- When the connection drops, Vayu asks for the remaining bytes. If the server provided a strong
  `ETag`, or a strong `Last-Modified` date, it is sent as `If-Range`. A `--checksum` also allows
  continuing, because publication then depends on the final digest. Without either check, Vayu
  starts over rather than risk mixing two versions of a file. Each partial response must have a
  matching `Content-Range`, length, and validators.
- If the file changes on the server mid-download, Vayu stops with a conflict (exit 7) and keeps
  the partial data. It does not merge the two versions.
- Automatic decompression is off (`Accept-Encoding: identity`). You get exactly the bytes the
  server sends. `Range`, `If-Range`, the conditional headers, and `Accept-Encoding` stay under
  Vayu's control; trying to set them with `--header` is an error.
- After a successful download, Vayu reports the SHA-256 it computed. It says "verified" only
  when you supplied the expected digest.

## Parallel connections

Vayu does not assume that more connections are faster. A single stream is often as fast, and
more connections can be refused or throttled. So every download starts with one plain
request, and segmentation is considered only when:

- the server reported the size;
- every range can be bound to the same version of the file: a strong validator (`ETag` or a
  strong `Last-Modified`, sent as `If-Range`), or a `--checksum` that gates publication;
- the response is not compressed;
- at least four checkpoint blocks (4 MiB or more) remain, and `--max-connections` is above 1.

Otherwise the download stays on one stream from start to end, exactly as with
`--max-connections 1`.

When segmentation is possible, Vayu measures the throughput of the first connection, then
tries one more. The new connection asks for the back half of the remaining work with a closed
range (`Range: bytes=A-B` and `If-Range`). Its response is checked like any resumed response
(status 206, matching `Content-Range`, size, and validators) before it takes any work from the
first stream. Vayu keeps the connection only if the combined throughput improves by at least
10% once the new connection has ramped up; otherwise it retires it and waits before trying
again, longer after each attempt that did not help. Measurement windows grow with the
server's response time, so high-latency links are judged on longer samples. More connections
are not added when less than about two seconds of transfer (or eight round trips) remain, or
when the disk cannot keep up.

`--min-connections N` sets a floor for these decisions: a segmented download starts with N
connections and never drops below N because extra connections did not raise throughput. It
still grows past N while connections help. The floor applies only to downloads that can be
segmented (above); others stay on one stream. Server limits (below) may still take it under N;
after the cooldown it returns to N directly.

- **Server limits.** HTTP 429 or 503 on an extra connection halves the connection count. A
  network failure, stall, or early close removes one connection. If the server ignores ranges
  (answers 200), rejects them (416), or refuses extra connections before it ever served a range,
  Vayu says so once and continues on one stream. That stream's work is never lost.
- **Changed source.** A response with different validators or a different size on any
  connection stops the whole download with a conflict (exit 7) and keeps the data.
- **One connection per worker.** Each connection slot has its own client and keeps its own
  connection between ranges, also over HTTP/2. A single HTTP/2 connection would carry every
  range over one congestion window and one per-connection limit, which defeats the purpose.
- **Fair limits.** `--limit-rate` applies to all connections together, and one retry budget
  covers the whole download: any connection's progress counts as progress.
- **No waste by design.** Work is split only at block boundaries, each byte has exactly one
  owner, and Vayu never downloads the same range twice on purpose. When a connection gives up
  part of its range, it stops at the boundary. At most the data already in flight is discarded.

The dashboard shows the number of active connections. `--ui detailed` adds the throughput of
each one. JSON progress events carry `connections`.

## Saved progress and resuming

While downloading, Vayu checkpoints about every 2 seconds, and also when a connection fails and
when it stops. A checkpoint first syncs `NAME.part` to disk with the platform's data sync. Then
it atomically replaces `NAME.vayu` with a new generation that claims
those bytes. `NAME.vayu` is readable only by you. It records:

- a hash of the URL you gave. For diagnostics it also keeps the URL's origin and file name, but
  never the rest of the path, the query, headers, cookies, or credentials;
- the file's size and validators (`ETag`, `Last-Modified`);
- any `--checksum` you supplied;
- the saved byte ranges, with a SHA-256 for each block of saved data.

To resume, run the same command again. Before sending any request, Vayu:

1. Locks the partial file, then checks the state file's own checksum and bounds. A damaged or
   unsupported state file is refused, and both files are left untouched.
2. Matches the URL to the one that was saved. A different URL is a conflict; use
   `--resume-from NAME.vayu` to continue with a refreshed link.
3. Re-reads the saved data and checks each block against its hash. Within each saved range,
   data after the first damaged block is downloaded again, as is data not covered by a
   checkpoint.
4. Asks the server for the missing ranges with `Range` and `If-Range`, then validates each
   response as during a single run. A download that used several connections usually leaves
   several saved ranges; only the gaps between them are requested.

Vayu continues only when it can prove continuity: the server provided a strong validator, or you
supplied a `--checksum` that gates publication. If it cannot, or if the file changed on the
server, or the server no longer honors range requests, Vayu stops with exit code 7. It keeps the
saved data and tells you the next step, usually `--checksum` or `--restart`. Within a single
run, Vayu still restarts from zero on its own when the server ignores a range request, because
then no data from an earlier run is at stake.

Mirrors often hold their own copy of a file, with its own `ETag` and `Last-Modified`. When the
server starts sending a copy during a transfer or a resume with other validators but the same size, Vayu downloads up to 8
saved blocks again from it, spread over the saved data. If every block matches its
saved hash, Vayu says so, binds the download to the new copy, and continues. If any block
differs, the file changed, and Vayu stops as above. Extra connections that reach such a copy
step back until the main connection has checked it.

When a signed link expires, the server answers 403. Vayu keeps the data and prints the
`--resume-from` command to use once you have a new link:

```sh
vayu "https://example.com/new-signed-url" --resume-from file.zip.vayu
```

Finalization is crash-safe too:

1. The last checkpoint records the complete size.
2. The checksum is verified, if one was given.
3. The file is linked into place.
4. `NAME.part` and `NAME.vayu` are removed.

If a run is interrupted after step 1, the next run finishes the work without downloading again.
If the final file is already in place, Vayu checks it against the saved block hashes and only
removes the leftovers. If the expected checksum was wrong, rerunning with the correct
`--checksum` finishes without downloading again.

What is guaranteed: the saved progress survives the process being stopped, crashing, or being
killed with `kill -9`. The tests check this, including injected crashes at every step of the
checkpoint and finalization protocols. Surviving power loss also depends on the storage honoring
sync requests; that is not tested here. Data received after the last checkpoint may be
downloaded again.

## Terminal, SSH, and headless use

- **Interactive terminal** (stderr is a TTY and `TERM` is set): a live dashboard of about eight
  lines in color. It shows real progress, smoothed speed with a short speed history, ETA (or `—`
  when it cannot be estimated honestly), connection state, and how long ago progress was last
  saved. It redraws up to 8 times a
  second locally and at most 2 times a second when Vayu detects an SSH session. It never uses an
  alternate screen, raw mode, or a hidden cursor, so scrollback stays intact. When the download
  ends, the dashboard is replaced by a summary.
- **Narrow windows** switch to a two-line layout, and very small windows to a single line.
  Resizing takes effect on the next frame.
- **Background jobs** (`vayu URL &`, or Ctrl+Z then `bg`) draw nothing over your shell. `fg`
  brings the dashboard back.
  - With `stty tostop`, a successful download finishes without output.
  - A failure message is still written. The job therefore stops, and the message appears
    when you run `fg`.
- **`TERM=dumb`, an unknown terminal, pipes, redirected files, cron, or SSH without a PTY**:
  plain `vayu: …` lines with no cursor movement and no automatic color. Progress is printed
  every 10% or once a minute, so log size stays bounded.
- **Color**: `--color` or the config value wins. Then a non-empty `NO_COLOR` disables color.
  Otherwise Vayu uses color only on a capable terminal. The baseline is the 16 ANSI colors,
  which follow your terminal theme; 256-color and truecolor palettes are used only when they
  are advertised and the background is known. Every status is also spelled out in text.
- **Glyphs**: Unicode with a UTF-8 locale; ASCII otherwise and on the Linux virtual console.
  Color still works with ASCII glyphs.
- Filenames, URLs, and server messages are stripped of control and bidi-override characters
  before display. Signed query values and credentials are redacted from all output.

Vayu downloads to the machine it runs on. To keep a download running after you log out of SSH,
use `tmux` or `screen`, or detach it:

```sh
nohup vayu "https://example.com/big.iso" --ui off > vayu.log 2>&1 < /dev/null &
```

When started under `nohup`, Vayu keeps `SIGHUP` ignored. Otherwise `SIGINT`, `SIGTERM`, and
`SIGHUP` stop it safely: pending data is written, synced, and checkpointed, and Vayu exits with
130, 143, or 129. Run the same command again to resume. A second signal exits immediately and
relies on the last checkpoint. If your SSH session drops and takes Vayu with it, log back in and
rerun the command. Your host's session policy, such
as systemd-logind's `KillUserProcesses`, can still end processes at logout.

[docs/COMPATIBILITY.md](docs/COMPATIBILITY.md) has the full terminal, session, and platform
matrix, and the test behind each entry.

## Proxies and credentials

Precedence: `--no-proxy` → `--proxy` → `proxy` in the config file (a URL, `system`, or `none`)
→ the environment (`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and lowercase forms; macOS also uses
its system proxy settings). `NO_PROXY` applies to environment proxies and also to an explicit
`--proxy`, as in curl.

Custom headers are sent only to the origin of the URL you gave. After a redirect to another
host, they are dropped. Imported cookies follow their domain, path, `Secure`, and expiry rules,
including across redirects. Redirects are limited to 10 and to `http`/`https`.

## Automation

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Internal error |
| 2 | Invalid arguments or configuration |
| 3 | Network failure or retry budget exhausted |
| 4 | Server, proxy, authentication, or TLS failure |
| 5 | Local storage failure |
| 6 | Integrity failure (checksum mismatch) |
| 7 | Conflict: the destination exists, saved progress belongs to another URL or is damaged, the source changed, or saved progress cannot be continued safely |
| 129 / 130 / 143 | Stopped by `SIGHUP` / `SIGINT` / `SIGTERM` |
| 141 | The `--json` reader closed stdout; the download stops and its progress is saved |

### JSON events (`--json`)

Each line is one JSON object with `"v": 1`, `"event"`, and `"time_ms"` (Unix milliseconds).
Nothing is styled. Events:
`start {url}`, `redirect {url}`,
`response {output, total_bytes, http_version, resumable}`,
`progress {received_bytes, saved_bytes, total_bytes, speed_bps, eta_ms, elapsed_ms, connections}`
(at most once a second; `saved_bytes` is the latest durable checkpoint),
`recover {output, saved_bytes, total_bytes, discarded_bytes}` (continuing an earlier run),
`retry {attempt, delay_ms, reason}`, `resume {offset}`, `restart {reason}`,
`verify`, `finalize`, `notice {message}`. The last event is either
`complete {output, bytes, elapsed_ms, sha256, checksum_verified}` or
`error {category, exit_code, message, hint, partial, received_bytes}`.
URLs in events are redacted.

## Configuration

Optional TOML, read from `$VAYU_CONFIG`, or `$XDG_CONFIG_HOME/vayu/config.toml`, otherwise
`~/.config/vayu/config.toml` on Linux and `~/Library/Application Support/vayu/config.toml` on
macOS. Command-line options take precedence. Unknown keys are errors.

```toml
max-connections = 15
min-connections = 1
proxy = "socks5h://127.0.0.1:1080"   # or "system" / "none"
retry = "auto"                        # or "forever", or a number
connect-timeout = "30s"
stall-timeout = "60s"
limit-rate = "2M"
checkpoint-interval = "2s"            # time between checkpoints while data arrives (min 10ms)
http-version = "auto"
ca-cert = ["/etc/ssl/corp-root.pem"]
ui = "auto"
color = "auto"
theme = "auto"
glyphs = "auto"
animation = true
```

If the config file contains proxy credentials, keep it readable only by you (`chmod 600`).

## TLS trust

Certificates are always verified. Vayu uses the platform verifier: Security.framework on macOS,
and the system certificate store on Linux (`SSL_CERT_FILE` and `SSL_CERT_DIR` are honored). On
minimal Linux systems with no certificate store, it falls back to the bundled Mozilla root set.
Use `--ca-cert` for private or corporate certificate authorities.

## Development

```sh
make                       # list every task
make test                  # unit and end-to-end tests (needs loopback networking and PTYs)
make lint                  # formatting and clippy, as CI runs them
make bench                 # local benchmark scenarios, then their summary
bench/compare.sh URL 5     # one URL against curl and aria2 (after make release)
```

[CONTRIBUTING.md](CONTRIBUTING.md) describes the test layout and the compatibility promises;
[docs/RELEASING.md](docs/RELEASING.md) describes the release gates.

The end-to-end tests run the real binary against a scripted local HTTP server. They cover
dropped connections, validator-bound resume, source changes, ignored ranges, redirects and
header scoping, cookies, retries with `Retry-After`, stalls, checksum mismatches, destination
conflicts, JSON output, and signals.

The recovery tests (`tests/recovery.rs`) cover:

- `SIGINT` and `kill -9` followed by a rerun;
- crashes injected at each checkpoint and finalization step;
- replacement URLs, and source changes between runs;
- damaged state files and damaged data blocks;
- servers that stop honoring ranges, and progress without a validator;
- two processes targeting one destination.

The terminal tests (`tests/terminal.rs`) run the binary on a pseudo-terminal. They cover:

- color modes and layouts, including resizes;
- the SSH refresh rate;
- dropped sessions, with and without `nohup`;
- background jobs.

Debug builds abort at a named step when `VAYU_FAULT=STEP` (or `STEP@N` for the Nth time) is
set; release builds contain no injection points. If IPv4 loopback is blocked on your machine
(some VPN and filtering software does this), the test server falls back to `[::1]`. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the module layout.

## Roadmap

1. **Foundation**: sequential HTTP/HTTPS, CLI, proxies and credentials, checksum verification,
   colored dashboard with plain and JSON fallbacks, exit codes, baseline benchmark script.
2. **Durable recovery**: a versioned `.vayu` state file with checkpoints, resume across runs,
   `--resume-from` for replacement URLs, and crash-safe finalization.
3. **Parallel engine**: validated range scheduling with bounded workers, positional writes,
   and one transport connection per worker.
4. **Adaptive performance**: a concurrency controller, range sizing by work stealing, stall
   recovery, and a local benchmark harness ([results](docs/BENCHMARKS.md)).
5. **Terminal and server compatibility**: PTY-tested color modes, layouts, SSH and background
   behavior ([matrix](docs/COMPATIBILITY.md)), and release packaging for macOS and Linux.
6. **Public beta** (tooling in place; first release candidate pending): a release process
   with checksums and provenance attestations ([RELEASING.md](docs/RELEASING.md)), Homebrew
   and script installers, contributor guidance, and a [changelog](CHANGELOG.md).

After the beta come wider real-network benchmarks (including aria2) and stabilizing the
command line, JSON events, and state format for 1.0.

Vayu makes no general performance claims until reproducible benchmarks against curl and aria2
on real networks back them up. [docs/BENCHMARKS.md](docs/BENCHMARKS.md) describes the local
harness, its synthetic conditions, and what the current results do and do not show.

## Privacy

Vayu has no telemetry, crash reporting, or update checks. It connects only to the URLs you give
it (and their redirects) and to the proxies you configure. See [SECURITY.md](SECURITY.md) for
reporting vulnerabilities and verifying releases.

## License

Copyright (c) 2026 Majid Hajiloo. Licensed under the [MIT license](LICENSE).

# Changelog

All notable changes to Vayu are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, minor versions may change the command
line, the JSON event schema, or the `.vayu` state format; each such change is listed below.

## Unreleased

The first public beta. Everything below is new.

### Downloading

- Single-binary HTTP/HTTPS downloader:
  - rustls TLS with the platform's trust store, plus `--ca-cert`;
  - HTTP/1.1 and HTTP/2;
  - HTTP, HTTPS, and SOCKS5 proxies, including system proxy settings;
  - extra request headers and `--header-file`;
  - Netscape `cookies.txt` files;
  - redirects with credential scoping;
  - SHA-256 verification with `--checksum`.
- Retries with exponential backoff and `Retry-After`, stall detection (`--stall-timeout`), and
  a retry policy (`--retry auto`, `forever`, or a count of consecutive failures).
- Adaptive parallel connections (`--max-connections`, default 15; `--min-connections`, default 1):
  - Every download starts on one connection.
  - Connections are added only while measurement shows they raise throughput, and removed when
    the server throttles them.
  - `--min-connections` keeps a floor of connections on segmented downloads even when more do
    not raise throughput; throttling may go below it until the cooldown passes. Without a
    maximum, a minimum above 15 raises the maximum to match.
  - Every partial response is validated against the source's identity. An error page sent
    with `200 OK` is retried, not mistaken for a new version of the file.
  - Vayu falls back to one connection when a server does not support ranges safely.
- A shared `--limit-rate` across all connections.

### Recovery

- Progress is saved durably in a versioned `.vayu` state file:
  - one SHA-256 per 64 KiB block;
  - at most a few seconds of work is lost on a crash or power loss.
- Running the same command again resumes from saved progress, but only when the server proves
  it is the same file (strong validator or `--checksum`). When a mirror sends its own copy
  under other validators at the same size, up to 8 saved blocks are downloaded again from it,
  and the download continues only if all of them match.
- Saved data that cannot be verified is downloaded again.
- `--resume-from` continues with a replacement URL, for example a newly signed link.
- `--restart` starts over.
- `SIGINT`, `SIGTERM`, and `SIGHUP` stop safely after saving progress; a second signal exits
  immediately.
- Finalization is crash-safe: the destination appears only when complete and verified.

### Terminal and automation

- A live dashboard in color:
  - 16-color, 256-color, and truecolor palettes, chosen by detection, `NO_COLOR`, and
    `--color`;
  - layouts for wide, narrow, and very small windows;
  - redraws at up to 8 frames per second locally and 2 per second over SSH;
  - no alternate screen.
- Plain line output for pipes, `TERM=dumb`, cron, and SSH without a PTY. Versioned JSON Lines
  events with `--json`.
- Background jobs (`vayu URL &`) don't draw over the shell. With `stty tostop`, a successful
  download is never stopped by `SIGTTOU`; only a failure message is written, and it shows on
  `fg`.
- Stable exit codes for scripts (see `vayu --help`).

### Distribution

- Release archives:
  - static Linux builds (musl) for x86-64 and ARM64;
  - glibc Linux builds;
  - macOS builds for Apple Silicon and Intel.
- `SHA256SUMS`, build provenance attestations, an installer that verifies checksums and
  refuses to replace an unrelated `vayu`, and Homebrew (`brew install majiidd/vayu/vayu`).
- `vayu --version` reports the crate name (`vayu`), so installers can tell Vayu from
  unrelated programs named `vayu`.
- No telemetry: Vayu contacts only the servers and proxies you give it.

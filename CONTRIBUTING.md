# Contributing to Vayu

Thank you for helping. Bug reports, compatibility reports, and pull requests are all welcome.

## Reporting problems

Every report should include:

- the output of `vayu --version` and your OS and architecture;
- the command you ran, with signed URLs, tokens, and cookies removed;
- the output, preferably with `--json` for download problems;
- for display problems: `TERM`, `COLORTERM`, the terminal emulator, and whether you were in
  SSH, tmux, or screen.

For a download that cannot be resumed, also include the `.vayu` file. It holds no file contents
and no credentials. It records:

- the URL's origin and file name (no path segments in between and no query);
- a hash of the request;
- sizes, server validators, and block hashes.

Security problems should not be reported in public issues; see [SECURITY.md](SECURITY.md).

## Development setup

`rust-toolchain.toml` pins the exact toolchain; rustup installs it on first use.

```sh
cargo build
cargo test                                   # unit and end-to-end tests
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo deny check                             # licenses and advisories (cargo install cargo-deny)
```

CI runs the same commands on macOS (ARM64 and Intel) and Linux (x86-64 and ARM64). A pull
request needs all of them to pass.

## Tests

| Location | What it covers |
|---|---|
| `src/**` unit tests | Parsing, state validation, range planning (including a randomized invariant check), the concurrency controller in simulation, layouts, palettes, and capability detection |
| `tests/download.rs` | The real binary against a scripted HTTP server: retries, redirects, resume, conflicts, JSON events, signals |
| `tests/recovery.rs` | Saved progress across runs: `kill -9`, injected crashes, damaged state and data, replacement URLs |
| `tests/parallel.rs` | Segmented transfers, fallback to one connection, throttling, recovery with gaps |
| `tests/terminal.rs` | The binary on a pseudo-terminal: dashboard and line output, color modes, layouts and resizes, the SSH refresh rate, hang-ups, background jobs |

How the end-to-end tests run:

- **Network:** they start an HTTP server on loopback. If IPv4 loopback is blocked, as some
  VPN and filtering software does, the server uses `[::1]`.
- **Sandboxes:** a sandbox that forbids sockets or pseudo-terminals makes these tests fail with
  a message instead of hanging.
- **Fault injection:** debug builds stop at named crash points when `VAYU_FAULT=STEP` (or
  `STEP@N`) is set. Release builds contain no injection points.

Rules for new tests:

- A behavior change comes with a test that fails without it.
- Prefer an end-to-end test when the behavior is visible to users.
- Tests must not depend on the internet or on timing tighter than a few hundred milliseconds.

## Benchmarks

`bench/` holds a local harness:

- `server.py` is a range server with per-connection, shared, and latency limits.
- `scenarios.sh` runs the comparison under each condition.
- `summarize.py` prints the median table.

Changes to the scheduler or the controller should include before-and-after numbers from
`bench/scenarios.sh 3`. See [docs/BENCHMARKS.md](docs/BENCHMARKS.md) for what these numbers
can and cannot show.

## Compatibility promises

These are user-facing contracts. Change them only deliberately, with a CHANGELOG entry:

- **Exit codes** (`vayu --help`).
- **The JSON event schema:** it is versioned; add fields freely, but changing or removing one
  needs a new schema version.
- **The `.vayu` state format:** it is versioned. A new version must keep refusing, with a
  clear message, the files it cannot read. It must never misread them.
- **Display detection:** `NO_COLOR` precedence, the plain output for pipes and `TERM=dumb`,
  and no alternate screen.

## Style

- Match the surrounding code. `cargo fmt` decides formatting.
- Comments explain why, not what.
- User-visible messages say what happened and what to do next.
- Never print credentials, cookies, or signed query values. Use the existing redaction
  helpers.
- No telemetry, update checks, or other network access beyond what the user asked for.

## Changelog

Add a line under `## Unreleased` in [CHANGELOG.md](CHANGELOG.md) for every user-visible
change.

## License

Vayu is licensed under the [MIT license](LICENSE). Unless you explicitly state otherwise, any
contribution you intentionally submit for inclusion is licensed under the same terms, without
any additional terms or conditions.

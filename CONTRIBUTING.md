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

`rust-toolchain.toml` pins the exact toolchain; rustup installs it on first use. The
`Makefile` holds every task, for you and for CI; `make` alone lists them.

```sh
make build
make test                    # unit and end-to-end tests; TEST=name filters
make test-parallel           # one end-to-end suite (also test-download, -recovery, -terminal)
make fmt-check
make clippy                  # warnings are errors
make test-release            # release script syntax, version gates, and notes
make deny                    # licenses and advisories (cargo install cargo-deny)
make check                   # all of the above
```

CI runs on every push, once per commit: a pull request from this repository shows its push
run's checks, and pull requests from forks get their own run. It checks formatting, Clippy, and
cargo-deny once on Linux, and runs the tests and release script checks on macOS ARM64 and Linux
(x86-64 and ARM64), plus Intel macOS on `main`, release tags, and manual runs. It caches
compiled dependencies and cancels a run when a newer push to the same branch supersedes it. It builds and packages all six release targets, verifies their linkage, runs Linux
distribution smoke tests, and tests the installer on macOS and musl Linux, including BusyBox
`sh`. It also assembles and verifies the combined release assets. A pull request needs all
jobs to pass. Version tags reuse this workflow before creating a draft release.

## Tests

| Location | What it covers |
|---|---|
| `src/**` unit tests | Parsing, state validation, range planning (including a randomized invariant check), the concurrency controller in simulation, layouts, palettes, and capability detection |
| `tests/download.rs` | The real binary against a scripted HTTP server: retries, redirects, resume, conflicts, JSON events, signals |
| `tests/recovery.rs` | Saved progress across runs: `kill -9`, injected crashes, damaged state and data, replacement URLs |
| `tests/parallel.rs` | Segmented transfers, fallback to one connection, throttling, recovery with gaps |
| `tests/terminal.rs` | The binary on a pseudo-terminal: dashboard and line output, color modes, layouts and resizes, the SSH refresh rate, hang-ups, background jobs |
| `scripts/test-release.sh` | Exact release-tag matching and changelog extraction |
| `scripts/test-installer.py` | Real release archives served over loopback: installation, upgrades, directory options, program conflicts, checksum failures, download failures, latest and pinned URLs |

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

To run the installer suite locally, build an archive for your machine, then run
`make test-installer`. For example, on Apple Silicon:

```sh
make release package TARGET=aarch64-apple-darwin
make test-installer                  # needs Python 3; installs only into temporary directories
```

On Linux, build the native musl target, then also run `make test-installer-busybox` with Docker.

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

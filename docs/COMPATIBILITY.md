# Compatibility

What Vayu supports, and what verifies each claim. "CI" means the check runs on every push in
[`ci.yml`](../.github/workflows/ci.yml) and again before every release in
[`release.yml`](../.github/workflows/release.yml).

## Platforms

| Platform | Release archive | Minimum | Verified by |
|---|---|---|---|
| macOS, Apple Silicon | `vayu-aarch64-apple-darwin` | macOS 11 | Full test suite on `macos-latest` (CI) |
| macOS, Intel | `vayu-x86_64-apple-darwin` | macOS 10.14 | Full test suite on `macos-15-intel` (CI) |
| Linux x86-64, any distribution | `vayu-x86_64-unknown-linux-musl` (static) | Linux 3.2 | Test suite on Ubuntu; binary run on Alpine, Debian oldstable and stable, and Fedora (CI) |
| Linux ARM64, any distribution | `vayu-aarch64-unknown-linux-musl` (static) | Linux 4.1 | Test suite on Ubuntu ARM64; the same distribution runs (CI) |
| Linux x86-64, glibc | `vayu-x86_64-unknown-linux-gnu` | glibc of Ubuntu 22.04 or older, printed by the release build | Runs on Ubuntu 22.04, Debian stable, and Fedora (release) |
| Linux ARM64, glibc | `vayu-aarch64-unknown-linux-gnu` | as above | as above |

Notes on the minimums:

- **macOS:** the minimums are what the binaries are built for (`MACOSX_DEPLOYMENT_TARGET`).
  - The Intel minimum is 10.14 because certificate verification uses
    `SecTrustEvaluateWithError`.
  - Only the current GitHub-hosted macOS versions are tested.
- **Linux kernels:** the minimums come from
  [Rust's platform support](https://doc.rust-lang.org/rustc/platform-support.html): kernel
  3.2+ for x86-64 and 4.1+ for ARM64.
  - Rust lists these for its glibc targets. The musl builds use the same standard library.
  - Only current kernels are tested.
- **Which Linux build to choose:** the static musl builds need no system libraries and are
  what the installer picks. Use them on older distributions.

Other Unix systems can build from source with `cargo install`; they are not tested. Windows is
not supported.

## Terminals

The display adapts to what the terminal on stderr can do. Detection reads only the environment
and the window size; it never sends queries to the terminal.

| Environment | Display | Verified by |
|---|---|---|
| xterm, iTerm2, Terminal.app, kitty, Alacritty, GNOME Terminal, WezTerm (`TERM=xterm-256color` or similar) | Live dashboard; 256 colors, or truecolor when `COLORTERM=truecolor` and the background is known | Unit matrix in `src/presentation/caps.rs`; PTY tests |
| tmux, screen (`TERM=tmux-256color`, `screen-256color`, `screen`) | Live dashboard; 256 or 16 colors | Unit matrix |
| `TERM=xterm`, `vt100`, or other 16-color terminals | Live dashboard; only the 8 base ANSI colors (`ESC[3Xm`) | Unit matrix; PTY test |
| Linux virtual console (`TERM=linux`) | Live dashboard in 16 colors; ASCII glyphs | Unit matrix |
| Non-UTF-8 locale (`LANG=C`) | ASCII glyphs; color unchanged | Unit matrix; PTY test |
| `NO_COLOR` set (non-empty) | No color unless `--color always` | Unit tests; PTY test |
| `TERM=dumb` or unset, on a terminal | Plain `vayu: …` lines; no escape sequences | PTY test |
| Pipes, files, cron, `ssh host vayu …` (no PTY) | Plain lines; progress every 10% or each minute | End-to-end tests |
| `--json` | JSON Lines on stdout | End-to-end tests |

Layouts:

| Window | Layout | Verified by |
|---|---|---|
| 50×12 or larger | Standard, about 8 lines; `--ui detailed` adds protocol, route, retries, and per-connection rates | PTY tests |
| Narrower or shorter | Two lines | PTY tests |
| Under 30 columns or 3 rows | One line | PTY tests |

Behavior during a download:

- **Resizes:** the next frame uses the new layout. Lines wrapped by a narrowing resize are
  cleared. *Verified by a PTY test that resizes mid-download.*
- **Redraw rate:** at most 8 frames a second locally and 2 over SSH (`SSH_CONNECTION`,
  `SSH_CLIENT`, or `SSH_TTY` set). Frames that would not change are not written. *Verified by
  a PTY test that compares the two rates.*
- **No full-screen modes:** there is no alternate screen, raw mode, or hidden cursor, so an
  abrupt exit leaves the terminal usable and the scrollback intact. *Verified by PTY tests.*

## Sessions and signals

| Situation | Behavior | Verified by |
|---|---|---|
| SSH session drops (terminal hangs up) | `SIGHUP`: progress is saved and Vayu exits with 129; rerun to resume | PTY test with a controlling terminal |
| Started with `nohup` | `SIGHUP` stays ignored; the download continues after the session drops | PTY test |
| Terminal closes but sends no signal | Output stops; the download completes | PTY test |
| Background job (`vayu URL &`, or Ctrl+Z then `bg`) | No live frames over the shell (tested); after `fg`, frames resume below the shell's output | PTY test with a job-control shell |
| Background job with `stty tostop` | Never stopped by `SIGTTOU` on success; a failure message is still written, so the shell shows the job as stopped until `fg` | PTY test |
| `SIGINT`, `SIGTERM` | Progress is saved; exit 130 or 143; a second signal exits at once | End-to-end tests |
| `kill -9`, crash, power loss | Rerun resumes from the last checkpoint after verifying saved blocks | Recovery tests |
| tmux or screen detached | The download continues; the dashboard redraws on reattach | Not automated; same code path as a terminal that stays open |

## Servers

Vayu speaks HTTP/1.1 and HTTP/2 over TLS 1.2 and 1.3, directly or through HTTP, HTTPS, and
SOCKS5 proxies. The end-to-end tests cover the server behaviors listed in the README's
Development section. They include servers that ignore ranges, change content mid-download,
throttle connections, or answer extra connections with 403 or 503.

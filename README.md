# Vayu — وایو

A command-line downloader for slow and unreliable connections. Vayu retries failed transfers,
adjusts parallel connections to improve speed, and saves progress so you can resume later.

Available for macOS and Linux. [Release notes](CHANGELOG.md).

## Install

Run this in your terminal:

```sh
curl -fsSL https://github.com/majiidd/vayu/releases/latest/download/install.sh | sh
```

The installer checks the download's checksum and installs into `~/.local/bin`. If `vayu` is
not found afterward, add this to your shell's startup file and open a new terminal:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Rerun the installer to update.

You can also download a binary archive from [GitHub Releases](https://github.com/majiidd/vayu/releases/latest),
extract it, and put `vayu` in a directory on your `PATH`. Choose macOS or Linux for your
architecture; the static musl build is recommended on Linux. See
[supported platforms](docs/COMPATIBILITY.md#platforms) and [download verification](SECURITY.md#verifying-downloads).

On macOS, if a browser-downloaded binary is blocked, clear its quarantine flag after
verification: `xattr -d com.apple.quarantine /path/to/vayu`.

## Download a file

```sh
vayu "https://example.com/file.zip"
```

Choose a file name or destination directory:

```sh
vayu "https://example.com/file.zip" -o ~/Downloads/archive.zip
vayu "https://example.com/file.zip" -o ~/Downloads/
```

Vayu shows progress, speed, and the estimated time remaining. Completed files are never
overwritten.

## Stop and resume

Press **Ctrl+C** to stop. Run the same command again to resume. Keep the `.part` and `.vayu`
files beside your download; they hold the partial data and saved progress.

If a download link expires, continue with a fresh URL:

```sh
vayu "https://example.com/new-link" --resume-from file.zip.vayu
```

To discard saved progress and start over, add `--restart`. If the server cannot safely resume
a download, Vayu keeps your saved data and explains the next step.

## Useful options

| Option | Use |
| --- | --- |
| `--limit-rate 2M` | Limit total download speed to 2 MiB/s |
| `--max-connections 4` | Use at most four connections; `1` uses a single stream |
| `--proxy socks5h://127.0.0.1:1080` | Download through a proxy |
| `--header-file headers.txt` | Read request headers from a file |
| `--cookie-file cookies.txt` | Use cookies in Netscape format |
| `--checksum sha256:HEX` | Verify the file against an expected SHA-256 digest |
| `--ui off` | Use plain progress output |
| `--json` | Output JSON Lines for scripts |
| `--quiet` | Show only errors |

For example:

```sh
vayu "https://example.com/file.zip" -o ~/Downloads/ --limit-rate 2M
```

Run `vayu --help` for all options.

## Over SSH

Vayu downloads onto the machine where it runs. Use `tmux` or `screen` to keep it running after
you disconnect, or start it with `nohup`:

```sh
nohup vayu "https://example.com/file.zip" --ui off > vayu.log 2>&1 < /dev/null &
```

## Help and privacy

[Report a problem](https://github.com/majiidd/vayu/issues) with your command, OS, and
`vayu --version` output. Remove private URLs, tokens, and cookies first.

Vayu has no telemetry or update checks. For private vulnerability reports, see
[SECURITY.md](SECURITY.md).

## For developers

Install [rustup](https://rustup.rs/); the repository pins its Rust toolchain.

```sh
git clone https://github.com/majiidd/vayu.git
cd vayu
make build
make ci       # formatting, Clippy, unit and integration tests, release script checks
```

Use `make` to list tasks, or `cargo install --path . --locked` to install your local build.
CI also checks dependencies, builds all six release targets, and tests packaged installers.

See [contributing](CONTRIBUTING.md), [architecture](docs/ARCHITECTURE.md), and
[releasing](docs/RELEASING.md) for details.

## License

Copyright (c) 2026 Majid Hajiloo. [MIT](LICENSE).

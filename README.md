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
[supported platforms](docs/COMPATIBILITY.md#platforms) and [download verification](docs/RELEASING.md#publish).

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

Use an owner-controlled destination directory. Vayu refuses directories or ancestors owned by
another user, non-sticky directories writable by other users, and unsafe writable ACL grants.
System sticky temporary directories are supported; new partial files are owner-only.

## Stop and resume

Press **Ctrl+C** to stop. Run the same command again to resume. Keep the `.part` and `.vayu`
files beside your download; they hold the partial data and saved progress.

If a download link expires, continue with a fresh URL:

```sh
vayu "https://example.com/new-link" --resume-from file.zip.vayu
```

To discard saved progress and start over, add `--restart`. If the server cannot safely resume
a download, Vayu keeps your saved data and explains the next step.

## All options

| Option | Use |
| --- | --- |
| `-o, --output PATH` | Save to a file or an existing directory |
| `--min-connections N` | Minimum connections for segmented downloads: `1`–`64`, default `1` |
| `--max-connections N` | Maximum simultaneous connections: `1`–`64`, default `15` or the minimum if higher |
| `--proxy URL` | Use an HTTP, HTTPS, SOCKS5, or SOCKS5H proxy |
| `--no-proxy` | Bypass configured, environment, and system proxies |
| `-H, --header NAME:VALUE` | Add a request header; repeat for multiple headers |
| `--header-file PATH` | Read `Name: value` headers from a file; repeat for multiple files |
| `--cookie-file PATH` | Import cookies in Netscape/Mozilla format |
| `--ca-cert PATH` | Trust additional PEM certificates; repeat for multiple files |
| `--checksum sha256:HEX` | Verify the file against an expected SHA-256 digest |
| `--restart` | Discard an incomplete download of the same destination and start over |
| `--resume-from PATH` | Continue a saved download, including with a fresh URL |
| `--retry MODE` | Retry policy: `auto`, `forever`, or a consecutive failure count; default `auto` |
| `--connect-timeout DURATION` | Connection setup timeout, such as `30s`; default `30s` |
| `--stall-timeout DURATION` | Stop a connection after this long without data; default `60s` |
| `--limit-rate RATE` | Limit total bytes per second, such as `500K` or `2M` |
| `--http-version VERSION` | HTTP protocol: `auto`, `1.1`, or `2` |
| `--ui MODE` | Progress display: `auto`, `compact`, `detailed`, or `off` |
| `--color WHEN` | Color output: `auto`, `always`, or `never` |
| `--theme THEME` | Color palette: `auto`, `dark`, or `light` |
| `--glyphs SET` | Progress symbols: `auto`, `unicode`, or `ascii` |
| `--no-animation` | Disable spinners and animated decoration |
| `-q, --quiet` | Show only actionable failures |
| `--json` | Output JSON Lines for scripts |
| `-h, --help` | Print help |
| `-V, --version` | Print the version |

For example:

```sh
vayu "https://example.com/file.zip" -o ~/Downloads/ --limit-rate 2M
```

Run `vayu --help` for command-line help.

## Over SSH

Vayu downloads onto the machine where it runs. Use `tmux` or `screen` to keep it running after
you disconnect, or start it with `nohup`:

```sh
nohup vayu "https://example.com/file.zip" --ui off > vayu.log 2>&1 < /dev/null &
```

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

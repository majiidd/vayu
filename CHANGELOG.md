# Changelog

Notable changes to Vayu are recorded here.

This changelog follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and release versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0] - 2026-11-06

First stable release.

### Added

- HTTP/HTTPS downloads over HTTP/1.1 and HTTP/2, with verified TLS certificates,
  platform trust stores, and custom certificate authorities through `--ca-cert`.
- HTTP, HTTPS, and SOCKS5 proxy support, including system proxy settings.
- Custom request headers, header files, Netscape cookie files, and redirects that keep
  credentials scoped to their origin.
- SHA-256 checksum verification before publishing a completed download.
- Automatic retries with exponential backoff, `Retry-After` support, configurable retry
  budgets, and stalled-connection detection.
- Adaptive parallel downloads with configurable connection limits, validated byte ranges,
  fallback to a single stream, and a shared bandwidth limit. Connections double while each
  new one adds about as much as the others, then settle where the link saturates; gains are
  confirmed over a second measurement window before extra connections are kept.
- Durable, versioned `.vayu` checkpoints with block hashes and verified resumption after
  interruptions, crashes, or restarts.
- Recovery through replacement URLs with `--resume-from`, verification of saved data against
  mirrors with different validators, and an explicit `--restart` option.
- Safe shutdown on `SIGINT`, `SIGTERM`, and `SIGHUP`, plus crash-safe finalization that
  publishes the destination only when the download is complete and verified.
- An adaptive terminal dashboard with color modes, small-window layouts, SSH-aware refresh
  rates, and background-job support.
- Plain output for pipes and basic terminals, versioned JSON Lines events, and stable exit
  codes for automation.
- Prebuilt binary archives for macOS on Apple Silicon and Intel, and Linux on x86-64 and
  ARM64, including static musl and glibc builds.
- Installation through GitHub Releases using `install.sh` or binary archives, with
  `SHA256SUMS` and build provenance attestations. The installer verifies checksums, tests the
  binary before replacing anything, refuses to replace an unrelated program named `vayu`
  unless explicitly requested, and explains failures in detail (more with `--verbose`).
- Shared CI and release checks covering all supported targets, recovery, terminal behavior,
  binary packaging, and installation.

[Unreleased]: https://github.com/majiidd/vayu/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/majiidd/vayu/releases/tag/v1.0.0

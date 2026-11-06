# Releasing

The first stable release is **1.0.0**. Users install from
[`install.sh`](https://github.com/majiidd/vayu/releases/latest/download/install.sh) or download
a prebuilt binary archive from [GitHub Releases](https://github.com/majiidd/vayu/releases/latest).

## Automated checks

[`ci.yml`](../.github/workflows/ci.yml) runs on branch pushes and pull requests.
[`release.yml`](../.github/workflows/release.yml) calls that same workflow for version tags,
so a release cannot skip a CI gate:

- Formatting, Clippy with warnings as errors, the complete unit and end-to-end test suites,
  and release script tests run on macOS (Apple Silicon and Intel) and Linux (x86-64 and ARM64).
  Debug assertions stay enabled so the recovery suite exercises injected crash points.
- cargo-deny checks dependency licenses, advisories, and sources.
- All six release targets are built, checked for linkage, and packaged. Linux binaries run
  in distribution containers. The installer is tested against the packaged macOS and musl
  binaries; musl builds also run the suite under BusyBox `sh`.
- The archives are combined, every checksum is verified, the installer is included, and the
  assembled release is tested again. The tag must match `Cargo.toml` exactly, and the
  changelog must contain release notes.

After every job passes, a tag run attests the tested archives and creates a **draft** GitHub
release containing six `vayu-TARGET.tar.gz` archives, `SHA256SUMS`, and `install.sh`. A manual
workflow run produces workflow artifacts for review without creating a release.

The `Makefile` exposes the same tasks locally. For example:

```sh
make check
make release check-binary package TARGET=aarch64-apple-darwin
make test-installer
make check-tag TAG=v1.0.0
make release-notes TAG=v1.0.0
```

`make test-installer` needs Python 3 and installs only into temporary directories.
`make smoke` and `make test-installer-busybox` need Docker; the latter needs a native musl
archive in `dist`. Once all six archives and their `.sha256` files are in `dist`, `make dist`
assembles the release assets.

## Repository setup

Allow GitHub Actions to create releases. The publish job requests `contents: write`,
`id-token: write`, and `attestations: write` for its `GITHUB_TOKEN`; no separate publishing
token is required. Require all CI jobs to pass before merging into `main`.

The macOS binaries carry the linker's ad-hoc signature, verified during the build. There is no
Apple Developer ID signing or notarization. The installer downloads without browser quarantine
flags. After downloading in a browser, macOS users may need to clear quarantine on the verified
executable with `xattr -d com.apple.quarantine vayu`.

## Prepare the release

1. Set `version` in `Cargo.toml` to `1.0.0`, and run `cargo check --offline` to update the
   package entry in `Cargo.lock` without changing dependency versions.
2. Review the `1.0.0` section of `CHANGELOG.md`, including the release date. Keep
   `## Unreleased` for subsequent changes. Check that the README documents the supported
   installation methods and current behavior.
3. Merge the release commit into `main` and wait for every CI job to pass. Review the
   `release` workflow artifact: it must contain all six archives, `SHA256SUMS`, and `install.sh`.
4. Tag the tested commit and push the tag:

   ```sh
   git tag -a v1.0.0 -m "Vayu 1.0.0"
   git push origin v1.0.0
   ```

For an optional release candidate, set the package version to `1.0.0-rc.1` first, update the
lockfile, and tag that commit as `v1.0.0-rc.1`. Tags and package versions must match exactly,
including prerelease suffixes. Tags with a hyphen create prerelease drafts, which do not
replace the latest stable download. The final release needs its own version commit and tag.

## Publish

1. Wait for the tag workflow to pass and open its draft release. Review the release notes and
   verify that all eight assets are attached.
2. Verify the archives and provenance from the downloaded workflow artifact or draft assets:

   ```sh
   sha256sum -c SHA256SUMS          # macOS: shasum -a 256 -c SHA256SUMS
   gh attestation verify vayu-x86_64-unknown-linux-musl.tar.gz --repo majiidd/vayu
   ```

3. Publish the draft as the latest stable release. This activates the documented
   `releases/latest/download/install.sh` and binary archive links.
4. Confirm the public installation path from a clean environment:

   ```sh
   curl -fsSL https://github.com/majiidd/vayu/releases/latest/download/install.sh | sh
   ~/.local/bin/vayu --version
   ```

   It must report `vayu 1.0.0 (vayu)`. Check a remote download, interrupt it with Ctrl+C, and
   rerun to confirm resumption. Check the terminal presentation over SSH and inside tmux.

For later releases, use the new version throughout these steps and move the corresponding
`Unreleased` changes into a dated changelog section before tagging.

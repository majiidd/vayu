# Releasing

Releases are built by [`release.yml`](../.github/workflows/release.yml) from a version tag.
Its build, check, package, and asset steps are `Makefile` targets, so they can be rerun
locally: for example `make release package TARGET=aarch64-apple-darwin`, or
`make check-tag TAG=v0.2.0-rc.1`. `make smoke` and `make test-installer` need Docker.
Attestation, uploads, and creating the release stay in the workflow.

1. **The workflow:** for each tag it
   - runs the full test matrix and cargo-deny, then builds and packages all six targets;
   - opens a **draft** GitHub release carrying:
     - the archives, `SHA256SUMS`, `install.sh`, and `vayu.rb`;
     - build provenance attestations.
2. **The maintainer:** checks the draft against the gates below, then publishes it by hand.
3. **Homebrew:** publishing a final (non-pre)release runs
   [`homebrew.yml`](../.github/workflows/homebrew.yml). It:
   - checks the release's `vayu.rb` against the archives;
   - commits the formula to the tap `majiidd/homebrew-vayu`;
   - installs it with `brew install majiidd/vayu/vayu` and runs `brew test` on macOS.

A tag with a hyphen (`v0.2.0-rc.1`) becomes a prerelease. Every release starts with at least
one release candidate.

## One-time setup

- **Releases tab:** let GitHub Actions create releases. The workflow's `GITHUB_TOKEN` needs
  `contents: write`, which the publish job requests.
- **Homebrew tap:**
  1. Create the public repository `majiidd/homebrew-vayu` with a `Formula/` directory.
  2. Create a fine-grained personal access token with **Contents: read and write** on that
     repository only.
  3. Save the token as the `HOMEBREW_TAP_TOKEN` secret of `majiidd/vayu`.
- **macOS signing:** none. There is no Apple Developer ID.
  - The macOS binaries carry the linker's ad-hoc signature, which Apple Silicon requires and
    which the release build verifies.
  - Homebrew and the installer download without quarantine, so Gatekeeper does not block
    them.
  - Users who download an archive in a browser must clear the quarantine flag:
    `xattr -d com.apple.quarantine vayu`.

## Cut a release candidate

1. **Update the files:**
   - Set `version` in `Cargo.toml`, e.g. `0.2.0`, and run `cargo check` so that `Cargo.lock`
     follows.
   - In `CHANGELOG.md`, rename `## Unreleased` to `## [0.2.0] - YYYY-MM-DD` and make sure it
     lists every user-visible change. State format, JSON schema, and exit code changes get their
     own entries.
   - Update the README status if the milestone changed.
2. **Merge to `main`** and wait for CI to pass.
3. **Tag the candidate:** `git tag -a v0.2.0-rc.1 -m "Vayu 0.2.0-rc.1"`, then
   `git push origin v0.2.0-rc.1`.

## Gates

A candidate becomes the release only if all of these hold. Record the results in the release
notes or the pull request.

1. **Tests:** the workflow's test jobs passed on macOS (ARM64 and Intel) and Linux (x86-64 and
   ARM64). They include:
   - the recovery suite (`tests/recovery.rs`): signals, `kill -9`, injected crashes, damaged
     state, and changed sources;
   - the terminal suite (`tests/terminal.rs`): PTY layouts, color modes, SSH rate, hang-ups,
     and background jobs.
2. **Platforms:** every build job passed its linkage check and distribution smoke tests. The
   publish job installed the musl archive with `install.sh` under BusyBox `sh`.
3. **Benchmarks reproduce:**
   - Download the `benchmark` artifact, or run `make bench` on a quiet machine.
   - Summarize the artifact with `make bench-summary BENCH_OUT=bench.txt`; `make bench`
     prints the summary itself. It must report no `PROBLEM` lines, so every run produced the
     correct file.
   - Compare it with [BENCHMARKS.md](BENCHMARKS.md). The pattern must hold:
     - vayu clearly beats vayu-1 in the per-connection scenarios;
     - it stays within noise of vayu-1 in the shared and no-ranges scenarios;
     - repeated transfer stays within a few blocks.
   - If the machine or the defaults changed, update BENCHMARKS.md in the release commit.
4. **Artifacts verify.** On a clean machine:

   ```sh
   gh release download v0.2.0-rc.1 --repo majiidd/vayu --pattern 'vayu-*' --pattern SHA256SUMS
   sha256sum -c SHA256SUMS            # shasum -a 256 -c SHA256SUMS on macOS
   gh attestation verify vayu-x86_64-unknown-linux-musl.tar.gz --repo majiidd/vayu
   ```

5. **Hands-on check:** a real download from a remote server. Then:
   - interrupt it with Ctrl+C and resume it;
   - repeat over SSH;
   - repeat inside tmux.

If a gate fails, fix it on `main` and tag the next candidate (`-rc.2`).

## Publish

1. **Tag the release:** tag the same commit as the last good candidate with the final version
   (`v0.2.0`), and wait for its draft.
2. **Publish the draft:** check the draft's notes, then publish it.
3. **Homebrew:** confirm that the Homebrew workflow passed and that
   `brew install majiidd/vayu/vayu` installs the new version.
4. **crates.io:** run `make publish-crate` from the tagged commit.
5. **Next cycle:** add a fresh `## Unreleased` heading to `CHANGELOG.md`.

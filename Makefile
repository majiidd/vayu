# Development, CI, and release tasks. `make` alone lists them.
#
# Works with GNU Make 3.81 (macOS) and later. Variables:
#
#   TARGET    Rust target triple for release, package, check-binary, smoke (default: host)
#   DIST      directory for release archives (default: dist)
#   TEST      test name filter for the test targets, e.g. make test TEST=resume
#   ARGS      arguments for make run
#   TAG       release tag, e.g. v1.0.0, checked against Cargo.toml
#   IMAGES    container images for make smoke
#   LOCKED    set empty to let cargo update Cargo.lock (default: --locked)

SHELL := /bin/bash
.DEFAULT_GOAL := help

CARGO ?= cargo
LOCKED ?= --locked
TARGET ?=
DIST ?= dist
TEST ?=
ARGS ?=
TAG ?=

CARGO_VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
# Release tags must match Cargo.toml exactly, including any prerelease suffix.
VERSION = $(if $(TAG),$(patsubst v%,%,$(TAG)),$(CARGO_VERSION))
TARGET_FLAG = $(if $(TARGET),--target $(TARGET))
BIN = target/$(if $(TARGET),$(TARGET)/)release/vayu
SHA256SUM := $(shell command -v sha256sum >/dev/null 2>&1 && echo sha256sum || echo shasum -a 256)
# Distributions for make smoke: musl binaries run everywhere, glibc ones need a recent enough libc.
IMAGES ?= $(if $(findstring -musl,$(TARGET)),alpine:latest debian:oldstable-slim debian:stable-slim fedora:latest,ubuntu:22.04 debian:stable-slim fedora:latest)

.PHONY: help build release run install clean toolchain \
	test test-unit test-download test-recovery test-parallel test-terminal test-release fmt fmt-check clippy lint deny ci check \
	musl-deps check-binary smoke package checksums dist test-installer test-installer-busybox \
	check-tag version release-notes

help: ## List the targets
	@awk 'BEGIN { FS = ":.*## " } \
		/^## / { printf "\n%s\n", substr($$0, 4) } \
		/^[a-z][a-z0-9 -]*:.*## / { printf "  %-16s %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

## Build and run

build: ## Debug build
	$(CARGO) build $(LOCKED)

release: ## Release build, for TARGET if set (adds the target with rustup)
	$(if $(TARGET),rustup target add $(TARGET))
	$(CARGO) build --release $(LOCKED) $(TARGET_FLAG)

run: ## Run the debug build with ARGS, e.g. make run ARGS="https://example.com/f"
	$(CARGO) run $(LOCKED) -- $(ARGS)

install: ## Install vayu into ~/.cargo/bin
	$(CARGO) install --path . $(LOCKED)

clean: ## Remove build output and release archives
	$(CARGO) clean
	rm -rf $(DIST)

toolchain: ## Show the pinned toolchain (rust-toolchain.toml), installing it if needed
	rustup show active-toolchain

## Checks

test: ## Unit and end-to-end tests (TEST filters by name)
	$(CARGO) test --all-targets $(LOCKED) $(TEST)

test-unit: ## Unit tests only
	$(CARGO) test $(LOCKED) --bin vayu $(TEST)

test-download test-recovery test-parallel test-terminal: test-%: ## One end-to-end suite, tests/NAME.rs
	$(CARGO) test $(LOCKED) --test $* $(TEST)

test-release: ## Check release scripts and version gates
	@for script in scripts/*.sh; do sh -n "$$script" || exit 1; done
	scripts/test-release.sh

fmt: ## Format the code
	$(CARGO) fmt

fmt-check: ## Fail if the code is not formatted
	$(CARGO) fmt --check

clippy: ## Lints, warnings as errors
	$(CARGO) clippy --all-targets $(LOCKED) -- -D warnings

lint: fmt-check clippy ## Formatting and lints

deny: ## Licenses and advisories (cargo install cargo-deny)
	$(CARGO) deny check

ci: lint test test-release ## What every CI test job runs

check: ci deny ## Everything a pull request needs

## Release

musl-deps: ## Install the musl toolchain packages (Debian/Ubuntu, needs sudo)
	sudo apt-get update && sudo apt-get install -y musl-tools cmake

check-binary: ## Check the linkage and version of the TARGET release binary
	scripts/check-binary.sh $(BIN) $(TARGET)

smoke: ## Run the TARGET release binary in each of IMAGES (needs Docker)
	for image in $(IMAGES); do docker run --rm -v "$(abspath $(BIN)):/usr/local/bin/vayu:ro" "$$image" vayu --version || exit 1; done

package: ## Archive the TARGET release binary into DIST
	$(if $(TARGET),,$(error package needs TARGET, e.g. make package TARGET=aarch64-apple-darwin))
	scripts/package.sh $(TARGET) $(DIST)

checksums: ## Collect DIST's archive checksums into DIST/SHA256SUMS and verify them
	cd $(DIST) && cat ./*.tar.gz.sha256 > SHA256SUMS && rm ./*.tar.gz.sha256 && $(SHA256SUM) -c SHA256SUMS

dist: check-tag checksums ## Release assets in DIST: archives, SHA256SUMS, and install.sh
	cp scripts/install.sh $(DIST)/install.sh

test-installer: ## Test install, upgrades, conflicts, and checksum failures from DIST (needs Python 3)
	scripts/test-installer.sh $(DIST)

test-installer-busybox: ## Run the installer suite under BusyBox sh (needs Docker)
	docker run --rm -v "$(CURDIR):/work:ro" -w /work python:3-alpine sh scripts/test-installer.sh $(DIST)

check-tag: ## Fail if TAG does not match the version in Cargo.toml
	@if [ -n "$(TAG)" ] && [ "$(TAG)" != "v$(CARGO_VERSION)" ]; then \
		echo "tag $(TAG) does not match Cargo.toml version $(CARGO_VERSION)" >&2; exit 1; \
	fi

version: ## Print the release version (from TAG, else Cargo.toml)
	@echo $(VERSION)

release-notes: ## Print the CHANGELOG.md section for VERSION
	@scripts/release-notes.sh $(VERSION)

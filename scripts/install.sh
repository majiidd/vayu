#!/bin/sh
# Install the vayu executable from a GitHub release, without administrator rights.
#
#   curl -fsSL https://github.com/majiidd/vayu/releases/latest/download/install.sh | sh
#   sh install.sh [--version X.Y.Z] [--dir DIR] [--force]
#
# Picks the archive for this machine (static musl builds on Linux), verifies it against the
# release's SHA256SUMS before installing anything, and installs into ~/.local/bin (or --dir,
# or $VAYU_INSTALL_DIR). If a different program named `vayu` is already installed there or
# first on PATH, the installer stops unless --force is given.
#
# Environment:
#   VAYU_REPO        GitHub owner/name to download from (default: majiidd/vayu)
#   VAYU_BASE_URL    download from this URL instead of GitHub (it holds the archives and
#                    SHA256SUMS), e.g. a mirror or a local test server
#   VAYU_INSTALL_DIR default installation directory
set -eu

VAYU_REPO=${VAYU_REPO:-majiidd/vayu}

say() { printf 'vayu-install: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

version=
dir=${VAYU_INSTALL_DIR:-$HOME/.local/bin}
force=0
while [ $# -gt 0 ]; do
  case $1 in
    --version) [ $# -ge 2 ] || die "--version needs a value"; version=${2#v}; shift 2 ;;
    --dir) [ $# -ge 2 ] || die "--dir needs a value"; dir=$2; shift 2 ;;
    --force) force=1; shift ;;
    -h | --help) sed -n '2,17p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
done

case $(uname -s) in
  Linux) os=unknown-linux-musl ;;
  Darwin) os=apple-darwin ;;
  *) die "no prebuilt vayu for $(uname -s); build from source with cargo" ;;
esac
case $(uname -m) in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) die "no prebuilt vayu for $(uname -m); build from source with cargo" ;;
esac
# A shell running under Rosetta reports x86_64 on Apple Silicon; prefer the native build.
if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ] &&
  [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ]; then
  arch=aarch64
fi
target=$arch-$os
asset=vayu-$target.tar.gz

if [ -n "${VAYU_BASE_URL:-}" ]; then
  base=${VAYU_BASE_URL%/}
else
  if [ -n "$version" ]; then
    base=https://github.com/$VAYU_REPO/releases/download/v$version
  else
    base=https://github.com/$VAYU_REPO/releases/latest/download
  fi
fi

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl --proto '=https,http' -fsSL --retry 3 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    die "curl or wget is required"
  fi
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    die "sha256sum or shasum is required to verify the download"
  fi
}

# True if FILE is a vayu built from this project.
ours() {
  "$1" --version 2>/dev/null | grep -q '(vayu)'
}

dest=$dir/vayu
existing=$(command -v vayu 2>/dev/null || true)
if [ "$force" = 0 ]; then
  for other in "$dest" "$existing"; do
    if [ -n "$other" ] && [ -e "$other" ] && ! ours "$other"; then
      die "$other is a different program named vayu; remove it, choose --dir, or pass --force"
    fi
  done
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading $asset from $base"
fetch "$base/$asset" "$tmp/$asset"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"
expected=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || die "SHA256SUMS has no entry for $asset"
actual=$(sha256 "$tmp/$asset")
[ "$expected" = "$actual" ] || die "checksum mismatch for $asset (expected $expected, got $actual)"

(cd "$tmp" && gzip -dc "$asset" | tar -xf -)
new=$tmp/vayu-$target/vayu
[ -x "$new" ] || die "the archive does not contain vayu-$target/vayu"
ours "$new" || die "the downloaded executable does not run on this machine"

mkdir -p "$dir"
# Copy next to the destination, then rename: an interrupted install never leaves a partial file.
cp "$new" "$dir/.vayu.install.$$"
chmod 755 "$dir/.vayu.install.$$"
mv -f "$dir/.vayu.install.$$" "$dest"
say "installed $("$dest" --version) to $dest"

case :$PATH: in
  *:"$dir":*)
    first=$(command -v vayu 2>/dev/null || true)
    if [ -n "$first" ] && [ "$first" != "$dest" ]; then
      say "note: $first comes first on PATH and will run instead"
    fi
    ;;
  *) say "note: $dir is not on PATH; add it, e.g. export PATH=\"$dir:\$PATH\"" ;;
esac

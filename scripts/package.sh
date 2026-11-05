#!/bin/sh
# Package a release build of vayu for one target.
#
# Usage: scripts/package.sh TARGET [OUT_DIR]
#
# Expects target/TARGET/release/vayu (build with `cargo build --release --locked --target
# TARGET`). Writes OUT_DIR/vayu-TARGET.tar.gz (default OUT_DIR: dist) and a matching
# .sha256 file in `sha256sum` format. The archive holds one directory, vayu-TARGET/, with the
# executable, README, changelog, and licenses. Asset names carry no version, so that
# `releases/latest/download/vayu-TARGET.tar.gz` always names the newest release.
set -eu

target=${1:?usage: scripts/package.sh TARGET [OUT_DIR]}
out=${2:-dist}
root=$(cd "$(dirname "$0")/.." && pwd)
bin="$root/target/$target/release/vayu"
name="vayu-$target"

[ -x "$bin" ] || { echo "package: $bin not found; build it first" >&2; exit 1; }

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}

mkdir -p "$out"
out=$(cd "$out" && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir "$stage/$name"
cp "$bin" "$stage/$name/vayu"
for doc in README.md CHANGELOG.md LICENSE; do
  cp "$root/$doc" "$stage/$name/"
done
chmod 755 "$stage/$name" "$stage/$name/vayu"
chmod 644 "$stage/$name"/*.md "$stage/$name"/LICENSE

# Only flags that GNU tar and bsdtar share; COPYFILE_DISABLE keeps macOS metadata out.
(cd "$stage" && COPYFILE_DISABLE=1 tar -cf - "$name" | gzip -9n >"$out/$name.tar.gz")
(cd "$out" && sha256 "$name.tar.gz" >"$name.tar.gz.sha256")
cat "$out/$name.tar.gz.sha256"

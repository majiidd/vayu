#!/bin/sh
# Check version gates and release-note extraction without publishing anything.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
version=$(make -s version TAG=)
make -s check-tag TAG="v$version"
for tag in "$version" "v$version.1" "v$version-rc.1" v99.0.0; do
  if make -s check-tag TAG="$tag" >/dev/null 2>&1; then
    echo "test-release: accepted a mismatched tag: $tag" >&2
    exit 1
  fi
done
scripts/release-notes.sh "$version" >/dev/null

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/scripts"
cp scripts/release-notes.sh "$tmp/scripts/"
cat > "$tmp/CHANGELOG.md" <<'CHANGELOG'
# Changelog
## Unreleased
Future changes.
## [1.0.0-rc.1] - 2026-11-06
Candidate notes.
## [1.0.0] - 2026-11-06
Stable notes.
## [0.9.0] - 2026-11-05
Older notes.
CHANGELOG
[ "$(sh "$tmp/scripts/release-notes.sh" v1.0.0)" = 'Stable notes.' ]
[ "$(sh "$tmp/scripts/release-notes.sh" 1.0.0-rc.1)" = 'Candidate notes.' ]
[ "$(sh "$tmp/scripts/release-notes.sh" 1.0.0-rc.2)" = 'Stable notes.' ]
if sh "$tmp/scripts/release-notes.sh" 99.0.0 >/dev/null 2>&1; then
  echo 'test-release: accepted a release without changelog notes' >&2
  exit 1
fi
echo 'Release script tests passed.'

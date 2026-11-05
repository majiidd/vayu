#!/bin/sh
# Print the CHANGELOG.md section for a version, for the GitHub release notes.
#
# Usage: scripts/release-notes.sh VERSION
#
# Matches a heading like `## 0.2.0 - 2026-12-01` or `## [0.2.0]`. A release candidate
# (0.2.0-rc.1) uses its own section if there is one, and otherwise the section of the version
# it leads up to.
set -eu

version=${1:?usage: scripts/release-notes.sh VERSION}
version=${version#v}
changelog=$(dirname "$0")/../CHANGELOG.md

section() {
  awk -v v="$1" '
    /^## / {
      if (found) exit
      heading = $2
      gsub(/[][]/, "", heading)
      found = heading == v
      next
    }
    found { print }
  ' "$changelog"
}

notes=$(section "$version")
[ -n "$notes" ] || notes=$(section "${version%%-*}")
[ -n "$notes" ] || {
  echo "release-notes: CHANGELOG.md has no section for $version; rename \"Unreleased\" first" >&2
  exit 1
}
printf '%s\n' "$notes"

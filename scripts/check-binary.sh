#!/bin/sh
# Check that a release build links as its target requires and is this project's vayu.
#
# Usage: scripts/check-binary.sh BIN [TARGET]
#
# musl builds must be static. glibc builds print the newest glibc symbol version they need.
# macOS builds print their minimum OS version and libraries, and must carry a valid signature
# (the linker's ad-hoc one is enough). Without TARGET, the host's kind is assumed.
set -eu

bin=${1:?usage: scripts/check-binary.sh BIN [TARGET]}
target=${2:-}
[ -x "$bin" ] || { echo "check-binary: $bin not found; build it first" >&2; exit 1; }

if [ -z "$target" ]; then
  case $(uname) in
    Darwin) target=host-darwin ;;
    *) target=host-gnu ;;
  esac
fi

file "$bin"
case $target in
  *-musl)
    if ldd "$bin" 2>&1 | grep -q '=>'; then ldd "$bin"; exit 1; fi ;;
  *-gnu)
    echo "Requires glibc $(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)" ;;
  *-darwin)
    otool -l "$bin" | grep -A3 -E 'LC_BUILD_VERSION|LC_VERSION_MIN' | grep -E 'minos|version'
    otool -L "$bin"
    # Apple Silicon requires a signature; the linker's ad-hoc signature permits execution.
    codesign --verify --verbose "$bin" ;;
esac
"$bin" --version | grep -q '(vayu)'

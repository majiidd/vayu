#!/bin/sh
# Install the vayu executable from a GitHub release, without administrator rights.
#
#   curl -fsSL https://github.com/majiidd/vayu/releases/latest/download/install.sh | sh
#   sh install.sh [--version X.Y.Z] [--dir DIR] [--force] [--verbose]
#
# Picks the archive for this machine (static musl builds on Linux), verifies it against the
# release's SHA256SUMS before installing anything, and installs into ~/.local/bin (or --dir,
# or $VAYU_INSTALL_DIR). If a different program named `vayu` is already installed there or
# first on PATH, the installer stops unless --force is given.
#
# Output goes to stderr. On a terminal it uses color (unless NO_COLOR is set) and a download
# progress bar; elsewhere it is plain text. Failures name what to check, and unexpected ones
# also print what a bug report needs.
#
# Environment:
#   VAYU_REPO        GitHub owner/name to download from (default: majiidd/vayu)
#   VAYU_BASE_URL    download from this URL instead of GitHub (it holds the archives and
#                    SHA256SUMS), e.g. a mirror or a local test server
#   VAYU_INSTALL_DIR default installation directory
set -eu

VAYU_REPO=${VAYU_REPO:-majiidd/vayu}

# Color only on a terminal and never with NO_COLOR; Unicode marks only in a UTF-8 locale.
style() {
  bold= dim= red= green= yellow= reset=
  if [ -t 2 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-dumb}" != dumb ]; then
    esc=$(printf '\033')
    bold="$esc[1m" dim="$esc[2m" red="$esc[31m" green="$esc[32m" yellow="$esc[33m"
    reset="$esc[0m"
  fi
  case ${LC_ALL:-${LC_CTYPE:-${LANG:-}}} in
    *[Uu][Tt][Ff]-8* | *[Uu][Tt][Ff]8*) ok='✓' bad='✗' ;;
    *) ok='+' bad='x' ;;
  esac
}

say() { printf '%s\n' "$*" >&2; }
field() { [ -z "$2" ] || printf '  %s%-12s%s %s\n' "$dim" "$1" "$reset" "$2" >&2; }
debug() { [ "$verbose" = 0 ] || field "$1" "$2"; }
busy() { printf '  %s%s%s\n' "$dim" "$*" "$reset" >&2; }
step() { printf '%s%s%s %s\n' "$green" "$ok" "$reset" "$*" >&2; }
note() { printf '%s!%s %s\n' "$yellow$bold" "$reset" "$*" >&2; }

# die MESSAGE [LABEL VALUE]...: stop with a message and the details that explain it.
die() {
  printf '%s%s error:%s %s\n' "$red$bold" "$bad" "$reset" "$1" >&2
  shift
  while [ $# -ge 2 ]; do
    field "$1" "$2"
    shift 2
  done
  if [ "$diagnose" = 1 ]; then
    field downloader "$(downloader_version)"
    [ -z "${https_proxy:-}${HTTPS_PROXY:-}${all_proxy:-}${ALL_PROXY:-}" ] ||
      field proxy "set in the environment"
    say ""
    say "If this keeps happening, report it with the output above:"
    say "  https://github.com/$VAYU_REPO/issues"
  fi
  exit 1
}

# Like die, for failures that may be a bug: adds what is needed to diagnose them.
fail() {
  diagnose=1
  die "$@"
}

interrupted() {
  printf '\n%s%s interrupted%s\n' "$red$bold" "$bad" "$reset" >&2
  exit "$1"
}

tilde() {
  case $1 in
    "$HOME"/*) printf '~/%s' "${1#"$HOME"/}" ;;
    *) printf '%s' "$1" ;;
  esac
}

size() {
  wc -c <"$1" | awk '{
    if ($1 >= 1048576) printf "%.1f MB", $1 / 1048576; else printf "%d KB", ($1 + 1023) / 1024
  }'
}

downloader_version() {
  "$downloader" --version </dev/null 2>&1 | awk 'NR == 1 { print; exit }'
}

# fetch URL FILE [progress]: download, explaining a failure in terms a user can act on.
fetch() {
  debug get "$1"
  status=0
  if [ "$downloader" = curl ]; then
    if [ "${3:-}" = progress ] && [ -t 2 ]; then
      curl --proto "$proto" --tlsv1.2 -fL --progress-bar --retry 3 -o "$2" "$1" || status=$?
    else
      curl --proto "$proto" --tlsv1.2 -fsSL --retry 3 -o "$2" "$1" || status=$?
    fi
  else
    wget -q -O "$2" "$1" || status=$?
  fi
  [ "$status" != 0 ] || return 0

  hint=
  case $downloader:$status in
    curl:22 | wget:8)
      why="the server returned an error"
      [ -n "${VAYU_BASE_URL:-}" ] ||
        hint="check that the release exists at https://github.com/$VAYU_REPO/releases" ;;
    curl:5 | curl:6)
      why="the host name could not be resolved"
      hint="check your network connection and proxy settings" ;;
    curl:7 | curl:18 | curl:28 | curl:52 | curl:55 | curl:56 | wget:4)
      why="the connection failed or was interrupted"
      hint="check your network connection, then run the installer again" ;;
    curl:35 | curl:60 | curl:77 | wget:5)
      why="a secure connection could not be established"
      hint="check the system clock and the installed CA certificates" ;;
    curl:1) why="the server redirected to an address that is not HTTPS" ;;
    *) why="$downloader failed" ;;
  esac
  fail "could not download ${1##*/}: $why" url "$1" status "$downloader exit status $status" \
    hint "$hint"
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

# The version of FILE if it is a vayu built from this project, else nothing. Its stdin is not
# ours to give: under `curl | sh` it is the rest of this script.
version_of() {
  "$1" --version </dev/null 2>/dev/null | awk '/\(vayu\)/ { print $2; exit }'
}

ours() {
  [ -n "$(version_of "$1")" ]
}

cleanup() {
  [ -z "$tmp" ] || rm -rf "$tmp"
  [ -z "$staged" ] || rm -f "$staged"
}

# Explain how to put the installation directory on PATH for the user's shell.
path_hint() {
  case $dir in
    "$HOME"/*) entry="\$HOME/${dir#"$HOME"/}" ;;
    *) entry=$dir ;;
  esac
  rc='~/.profile' line="export PATH=\"$entry:\$PATH\""
  case ${SHELL:-} in
    */zsh) rc='~/.zshrc' ;;
    */bash) if [ "$os" = apple-darwin ]; then rc='~/.bash_profile'; else rc='~/.bashrc'; fi ;;
    */fish) rc='~/.config/fish/config.fish' line="fish_add_path $entry" ;;
  esac
  note "$(tilde "$dir") is not on your PATH. Add this line to $rc, then open a new terminal:"
  say ""
  say "    $line"
}

# Everything runs from here, so a truncated `curl | sh` download runs nothing.
main() {
  style
  version= verbose=0 diagnose=0 target=
  dir=${VAYU_INSTALL_DIR:-$HOME/.local/bin}
  force=0
  while [ $# -gt 0 ]; do
    case $1 in
      --version) [ $# -ge 2 ] && [ -n "$2" ] || die "--version needs a value"; version=${2#v}; shift 2 ;;
      --dir) [ $# -ge 2 ] && [ -n "$2" ] || die "--dir needs a value"; dir=$2; shift 2 ;;
      --force) force=1; shift ;;
      -v | --verbose) verbose=1; shift ;;
      -h | --help)
        cat <<'HELP'
Usage: sh install.sh [--version X.Y.Z] [--dir DIR] [--force] [--verbose]

Install Vayu from GitHub Releases (latest stable release by default).
The archive's SHA-256 checksum is verified before installation.

  --version X.Y.Z  Install a specific release (an optional leading v is accepted)
  --dir DIR        Install into DIR (default: VAYU_INSTALL_DIR or ~/.local/bin)
  --force          Allow replacing another program named vayu
  -v, --verbose    Show download URLs, checksums, and tool versions
  -h, --help       Show this help

Environment: VAYU_REPO, VAYU_BASE_URL, VAYU_INSTALL_DIR, NO_COLOR.
HELP
        exit 0 ;;
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
  # Plain HTTP only when explicitly asked for, e.g. a local mirror; never by redirect.
  case $base in
    http://*) proto='=http,https' ;;
    *) proto='=https' ;;
  esac
  if command -v curl >/dev/null 2>&1; then
    downloader=curl
  elif command -v wget >/dev/null 2>&1; then
    downloader=wget
  else
    die "curl or wget is required"
  fi
  dest=$dir/vayu

  say "${bold}Vayu installer${reset}"
  field system "$(uname -srm) ($target)"
  field release "${version:-latest}"
  field source "$base"
  field destination "$(tilde "$dest")"
  debug downloader "$(downloader_version)"
  say ""

  [ ! -d "$dest" ] || die "$(tilde "$dest") is a directory" hint "remove it or choose --dir"
  existing=$(command -v vayu 2>/dev/null || true)
  if [ "$force" = 0 ]; then
    for other in "$dest" "$existing"; do
      if [ -n "$other" ] && [ -e "$other" ] && ! ours "$other"; then
        die "$(tilde "$other") is a different program named vayu" \
          hint "remove it, choose --dir, or pass --force"
      fi
    done
  fi
  previous=
  [ ! -e "$dest" ] || previous=$(version_of "$dest")

  tmp= staged=
  trap cleanup EXIT
  trap 'interrupted 130' INT
  trap 'interrupted 143' TERM
  tmp=$(mktemp -d)
  debug temp "$tmp"

  busy "Downloading $asset"
  fetch "$base/$asset" "$tmp/$asset" progress
  fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"
  step "Downloaded $asset ($(size "$tmp/$asset"))"

  expected=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
  [ -n "$expected" ] || fail "SHA256SUMS has no entry for $asset" url "$base/SHA256SUMS"
  actual=$(sha256 "$tmp/$asset")
  [ "$expected" = "$actual" ] || fail "checksum mismatch for $asset" \
    expected "$expected" actual "$actual" \
    hint "the download is damaged or was changed; run the installer again"
  step "Verified SHA-256 checksum"
  debug sha256 "$actual"

  (cd "$tmp" && gzip -dc "$asset" | tar -xf -)
  new=$tmp/vayu-$target/vayu
  [ -f "$new" ] && [ -x "$new" ] || fail "the archive does not contain vayu-$target/vayu"

  mkdir -p "$dir"
  dir=$(CDPATH= cd -- "$dir" && pwd)
  dest=$dir/vayu
  # Stage next to the destination, then rename: an interrupted install never leaves a partial
  # file. Try the staged copy there, since the temporary directory may be mounted noexec.
  staged=$(mktemp "$dir/.vayu.install.XXXXXX")
  debug staged "$staged"
  cp "$new" "$staged"
  chmod 755 "$staged"
  if ! ours "$staged"; then
    output=$("$staged" --version </dev/null 2>&1 | awk 'NR == 1 { print; exit }')
    fail "the downloaded executable does not run on this machine" output "$output" \
      hint "if $(tilde "$dir") does not allow running programs (noexec), choose another with --dir"
  fi
  mv -f "$staged" "$dest"
  installed=$(version_of "$dest")
  if [ -z "$previous" ]; then
    step "Installed vayu $installed to $(tilde "$dest")"
  elif [ "$previous" = "$installed" ]; then
    step "Reinstalled vayu $installed at $(tilde "$dest")"
  else
    step "Updated vayu from $previous to $installed at $(tilde "$dest")"
  fi

  say ""
  case :$PATH: in
    *:"$dir":*)
      first=$(command -v vayu 2>/dev/null || true)
      if [ -n "$first" ] && [ "$first" != "$dest" ]; then
        note "$(tilde "$first") comes first on PATH and will run instead"
      else
        say "Run ${bold}vayu --help${reset} to get started."
      fi
      ;;
    *) path_hint ;;
  esac
}

main "$@"

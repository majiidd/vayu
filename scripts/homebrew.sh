#!/bin/sh
# Print a Homebrew formula for a published release, for a tap repository.
#
# Usage: scripts/homebrew.sh VERSION OWNER/REPO SHA256SUMS > vayu.rb
#
# The formula installs the prebuilt archives (Apple Silicon and Intel macOS; static ARM64 and
# x86-64 Linux) and checks that the installed `vayu` is this project's.
set -eu

version=${1:?usage: scripts/homebrew.sh VERSION OWNER/REPO SHA256SUMS}
repo=${2:?usage: scripts/homebrew.sh VERSION OWNER/REPO SHA256SUMS}
sums=${3:?usage: scripts/homebrew.sh VERSION OWNER/REPO SHA256SUMS}
version=${version#v}

sum() {
  value=$(awk -v f="vayu-$1.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$sums")
  [ -n "$value" ] || { echo "homebrew: no checksum for vayu-$1.tar.gz in $sums" >&2; exit 1; }
  echo "$value"
}

url=https://github.com/$repo/releases/download/v$version
# Computed up front: a failure inside the here-document would not stop the script.
mac_arm=$(sum aarch64-apple-darwin)
mac_intel=$(sum x86_64-apple-darwin)
linux_arm=$(sum aarch64-unknown-linux-musl)
linux_intel=$(sum x86_64-unknown-linux-musl)

cat <<EOF
class Vayu < Formula
  desc "Resilient downloader for slow, unstable, and high-latency networks"
  homepage "https://github.com/$repo"
  version "$version"
  license "MIT"

  on_macos do
    on_arm do
      url "$url/vayu-aarch64-apple-darwin.tar.gz"
      sha256 "$mac_arm"
    end
    on_intel do
      url "$url/vayu-x86_64-apple-darwin.tar.gz"
      sha256 "$mac_intel"
    end
  end

  on_linux do
    on_arm do
      url "$url/vayu-aarch64-unknown-linux-musl.tar.gz"
      sha256 "$linux_arm"
    end
    on_intel do
      url "$url/vayu-x86_64-unknown-linux-musl.tar.gz"
      sha256 "$linux_intel"
    end
  end

  def install
    bin.install "vayu"
    doc.install "README.md", "CHANGELOG.md"
  end

  test do
    assert_match "(vayu-download)", shell_output("#{bin}/vayu --version")
  end
end
EOF

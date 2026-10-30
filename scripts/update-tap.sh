#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "Usage: scripts/update-tap.sh <version-without-v> <tap-dir>" >&2
  exit 1
fi

VERSION=$1
TAP_DIR=$2
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Version must be X.Y.Z without a leading v" >&2
  exit 1
fi

BASE_URL="https://github.com/dickwu/sub-bot/releases/download/v$VERSION"

checksum() {
  local target=$1 hex
  hex=$(curl -fsSL --retry 3 --retry-delay 2 "$BASE_URL/sub-bot-v$VERSION-$target.tar.gz.sha256" | awk '{print $1}')
  if [[ ! "$hex" =~ ^[[:xdigit:]]{64}$ ]]; then
    echo "Invalid SHA-256 checksum for $target" >&2
    return 1
  fi
  printf '%s' "$hex"
}

MACOS_ARM_SHA=$(checksum aarch64-apple-darwin)
MACOS_INTEL_SHA=$(checksum x86_64-apple-darwin)
LINUX_ARM_SHA=$(checksum aarch64-unknown-linux-musl)
LINUX_INTEL_SHA=$(checksum x86_64-unknown-linux-musl)

mkdir -p "$TAP_DIR/Formula"
cat > "$TAP_DIR/Formula/sub-bot.rb" <<EOF
class SubBot < Formula
  desc "Self-hosted Rust agent server with sessions, async tasks and signed webhooks"
  homepage "https://github.com/dickwu/sub-bot"
  version "$VERSION"

  on_macos do
    on_arm do
      url "https://github.com/dickwu/sub-bot/releases/download/v#{version}/sub-bot-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "$MACOS_ARM_SHA"
    end

    on_intel do
      url "https://github.com/dickwu/sub-bot/releases/download/v#{version}/sub-bot-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "$MACOS_INTEL_SHA"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/dickwu/sub-bot/releases/download/v#{version}/sub-bot-v#{version}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "$LINUX_ARM_SHA"
    end

    on_intel do
      url "https://github.com/dickwu/sub-bot/releases/download/v#{version}/sub-bot-v#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "$LINUX_INTEL_SHA"
    end
  end

  def install
    bin.install "sub-bot"
    pkgshare.install "sub-bot.example.json"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/sub-bot --version")
  end
end
EOF

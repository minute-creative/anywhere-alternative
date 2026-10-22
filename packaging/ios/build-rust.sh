#!/bin/bash
# Xcode "Build Rust viewer" phase: builds crates/aa-viewer as a static
# library for the iPhone, which the app links (see project.pbxproj).
#
# Runs cargo with a clean environment: Xcode exports SDKROOT and many other
# settings for the iPhone, which would also be applied to the small Mac
# programs cargo builds and runs along the way (build scripts) and break them.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET_IOS="${IPHONEOS_DEPLOYMENT_TARGET:-17.0}"
exec env -i HOME="$HOME" USER="${USER:-}" \
  PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  IPHONEOS_DEPLOYMENT_TARGET="$TARGET_IOS" \
  bash -c "cd '$ROOT' && rustup target add aarch64-apple-ios >/dev/null && \
    cargo rustc -p aa-viewer --lib --release --target aarch64-apple-ios --crate-type staticlib"

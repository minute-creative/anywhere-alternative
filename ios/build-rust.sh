#!/bin/bash
# Builds the Rust engine (crates/aa-mobile) for the iPhone or the simulator
# and puts the library where Xcode links it: ios/rust-lib/<platform>/.
# Xcode runs this before compiling the Swift code; it can also be run by
# hand: PLATFORM_NAME=iphoneos ./build-rust.sh
set -euo pipefail
cd "$(dirname "$0")/.."

# Xcode's script environment has no cargo on PATH.
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

platform="${PLATFORM_NAME:-iphoneos}"
archs="${ARCHS:-arm64}"

# One Rust target per processor Xcode asks for (the simulator on an Apple
# Silicon Mac is arm64; Xcode may ask for x86_64 as well).
targets=()
for arch in $archs; do
  case "$platform:$arch" in
    iphoneos:arm64) targets+=(aarch64-apple-ios) ;;
    iphonesimulator:arm64) targets+=(aarch64-apple-ios-sim) ;;
    iphonesimulator:x86_64) targets+=(x86_64-apple-ios) ;;
    *) echo "unknown platform/arch $platform/$arch" >&2; exit 1 ;;
  esac
done

# The C parts (OpenH264, Opus) need to know the lowest iOS we run on.
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-17.0}"
# Xcode exports SDKROOT for the app's SDK; cargo's C compiler would pick it
# up for the C code, but the Rust linker must not: use the right one.
unset SDKROOT

libs=()
for target in "${targets[@]}"; do
  if ! rustup target list --installed | grep -q "^$target$"; then
    rustup target add "$target"
  fi
  cargo build -p aa-mobile --release --target "$target"
  libs+=("target/$target/release/libaa_mobile.a")
done

out="ios/rust-lib/$platform"
mkdir -p "$out"
if [ "${#libs[@]}" -eq 1 ]; then
  cp "${libs[0]}" "$out/libaa_mobile.a"
else
  lipo -create "${libs[@]}" -output "$out/libaa_mobile.a"
fi
echo "Rust engine ready: $out/libaa_mobile.a (${targets[*]})"

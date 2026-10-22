#!/bin/bash
# Anywhere for iPhone: builds the Rust viewer and the Xcode app.
# Log: ~/anywhere-alternative/ios-build-log.txt
cd ~/anywhere-alternative || exit 1
exec > >(tee ios-build-log.txt) 2>&1
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
echo "== Anywhere iPhone build $(date) =="
chmod +x packaging/ios/build-rust.sh
echo "== 1/3 Mac viewer still builds (lib + command split) =="
cargo check -p aa-viewer 2>&1 | grep -E "^(error|warning)" -A6 | head -60
echo "== mac check exit: ${PIPESTATUS[0]}"
echo "== 2/3 Rust viewer for iPhone =="
SRCROOT="$PWD/packaging/ios" packaging/ios/build-rust.sh > /tmp/aa-ios-rust.log 2>&1
rc=$?
grep -E "^(error|warning)" -A8 /tmp/aa-ios-rust.log | head -150
tail -5 /tmp/aa-ios-rust.log
echo "== rust ios exit: $rc"
ls -la target/aarch64-apple-ios/release/libaa_viewer.a 2>&1
if [ $rc -eq 0 ]; then
  echo "== 3/3 Xcode app =="
  xcodebuild -project packaging/ios/AnywhereiOS.xcodeproj -scheme Anywhere -configuration Release \
    -destination 'generic/platform=iOS' -allowProvisioningUpdates build > /tmp/aa-ios-xcode.log 2>&1
  xrc=$?
  grep -E "error:|warning: .*(undefined|not found)|Undefined symbols|BUILD (SUCCEEDED|FAILED)" /tmp/aa-ios-xcode.log | head -60
  echo "== xcode exit: $xrc"
fi
echo "== BUILD DONE =="
echo "You can close this window."

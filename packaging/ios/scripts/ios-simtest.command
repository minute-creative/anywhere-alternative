#!/bin/bash
# Cross-test the Anywhere iPhone app in the iPhone Simulator on this Mac,
# watching this Mac (127.0.0.1). Saves sim-shot.png and sim-viewer.log.
# A pairing code for the simulator (it's a separate "phone") can be put in
# ~/anywhere-alternative/sim-pair-code.txt; it's used once.
cd ~/anywhere-alternative || exit 1
exec > >(tee ios-simtest-log.txt) 2>&1
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
BID=com.minutecreative.anywhere.ios
echo "== Anywhere simulator test $(date) =="
echo "== 1/4 Rust viewer for the Simulator =="
env -i HOME="$HOME" PATH="$PATH:/usr/bin:/bin:/usr/sbin:/sbin" IPHONEOS_DEPLOYMENT_TARGET=17.0 \
  bash -c "cd '$PWD' && rustup target add aarch64-apple-ios-sim >/dev/null && \
  cargo rustc -p aa-viewer --lib --release --target aarch64-apple-ios-sim --crate-type staticlib" > /tmp/aa-sim-rust.log 2>&1
rc=$?; grep -E "^error" -A8 /tmp/aa-sim-rust.log | head -40; echo "rust exit: $rc"; [ $rc -eq 0 ] || { echo "== SIMTEST FAILED =="; exit 1; }
echo "== 2/4 App for the Simulator =="
xcodebuild -project packaging/ios/AnywhereiOS.xcodeproj -target Anywhere -configuration Release -sdk iphonesimulator \
  ARCHS=arm64 SUPPORTED_PLATFORMS=iphonesimulator CODE_SIGNING_ALLOWED=NO \
  LIBRARY_SEARCH_PATHS="$PWD/target/aarch64-apple-ios-sim/release" SYMROOT="$PWD/target/ios-sim-build" \
  build > /tmp/aa-sim-xcode.log 2>&1
grep -E "error:|BUILD (SUCCEEDED|FAILED)" /tmp/aa-sim-xcode.log | head -20
APP="$PWD/target/ios-sim-build/Release-iphonesimulator/Anywhere.app"
[ -d "$APP" ] || { echo "== SIMTEST FAILED (no app) =="; exit 1; }
echo "== 3/4 Simulator =="
SIM=$(xcrun simctl list devices available | awk -F'[()]' '/iPhone/ {print $2; exit}')
NAME=$(xcrun simctl list devices available | grep -m1 iPhone | sed 's/ (.*//; s/^ *//')
echo "simulator: $NAME $SIM"
xcrun simctl boot "$SIM" 2>/dev/null; open -a Simulator; sleep 8
xcrun simctl terminate "$SIM" $BID 2>/dev/null
xcrun simctl install "$SIM" "$APP"
xcrun simctl spawn "$SIM" defaults write $BID host 127.0.0.1
xcrun simctl spawn "$SIM" defaults write $BID resolution 1080
if [ -s sim-pair-code.txt ]; then
  xcrun simctl spawn "$SIM" defaults write $BID pair_code "$(tr -dc 0-9 < sim-pair-code.txt)"; rm -f sim-pair-code.txt
fi
echo "== 4/4 Run =="
xcrun simctl launch "$SIM" $BID
sleep 25
xcrun simctl io "$SIM" screenshot sim-shot.png
DATA=$(xcrun simctl get_app_container "$SIM" $BID data)
cp "$DATA/Documents/viewer.log" sim-viewer.log 2>/dev/null
grep -E "starting|paired|window size|session|ERROR|stopped|negotiated" sim-viewer.log | cut -c1-220 | tail -15
echo "== SIMTEST DONE =="

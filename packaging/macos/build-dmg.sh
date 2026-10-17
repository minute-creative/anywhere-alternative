#!/bin/bash
# Builds Anywhere.app and Anywhere-<version>-mac.dmg (Apple Silicon).
# Run on a Mac from the repository root: packaging/macos/build-dmg.sh
set -euo pipefail
VERSION="${1:-$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)}"
OUT=dist
APP="$OUT/Anywhere.app"

cargo build --release -p aa-app -p aa-host -p aa-viewer

rm -rf "$OUT" && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/anywhere target/release/aa-host target/release/aa-viewer "$APP/Contents/MacOS/"
sed "s/__VERSION__/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"

# App icon: every size macOS wants, from the 1024 px master.
ICONSET="$OUT/AppIcon.iconset"
mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z $s $s assets/icon.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s*2)) $((s*2)) assets/icon.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$ICONSET"

# The browser add-on ("Anywhere Camera" in Chrome, Edge and Safari): the
# Chrome/Edge folder inside the app (for "Load unpacked"), the zip for the
# stores next to the disk image, and the Safari extension in PlugIns. The
# Safari part needs Xcode; if it fails, the rest of the app still ships.
python3 packaging/browser/build_addon.py "$VERSION"
cp -R dist/addon/chrome "$APP/Contents/Resources/browser-addon"
if ! packaging/macos/build-safari-ext.sh "$APP" "$VERSION"; then
  echo "warning: the Safari extension could not be built; shipping without it"
fi

# Signature. With MAC_SIGN_P12 (a fixed self-made certificate, stored as a
# GitHub secret) every version carries the same identity, so macOS keeps
# the Screen Recording / Accessibility permissions across updates. Without
# it: an ad-hoc signature (free, but macOS asks for the permissions again
# after each update). Either way: "Open Anyway" once on first install.
if [ -n "${MAC_SIGN_P12:-}" ]; then
  KC="$RUNNER_TEMP/sign.keychain-db"
  security create-keychain -p ci "$KC"
  security set-keychain-settings "$KC"
  security unlock-keychain -p ci "$KC"
  echo "$MAC_SIGN_P12" | base64 --decode > "$RUNNER_TEMP/sign.p12"
  security import "$RUNNER_TEMP/sign.p12" -k "$KC" -P "$MAC_SIGN_PASSWORD" -T /usr/bin/codesign
  security set-key-partition-list -S apple-tool:,apple: -s -k ci "$KC" >/dev/null
  security list-keychains -d user -s "$KC" $(security list-keychains -d user | tr -d '"')
  # A self-made certificate must be trusted for code signing on this build machine.
  security find-certificate -c "Anywhere Alternative Code Signing" -p "$KC" > "$RUNNER_TEMP/sign.pem"
  sudo security add-trusted-cert -d -r trustRoot -p codeSign -k /Library/Keychains/System.keychain "$RUNNER_TEMP/sign.pem"
  # Inside out: the Safari extension first (it must be sandboxed), then
  # the programs, then the app around them.
  for x in "$APP"/Contents/PlugIns/*.appex; do
    [ -e "$x" ] || continue
    codesign --force --sign "Anywhere Alternative Code Signing" --keychain "$KC" \
      --entitlements packaging/macos/safari-extension.entitlements "$x"
  done
  for b in aa-host aa-viewer anywhere; do
    codesign --force --sign "Anywhere Alternative Code Signing" --keychain "$KC" \
      --identifier "com.minutecreative.anywhere.$b" "$APP/Contents/MacOS/$b"
  done
  codesign --force --sign "Anywhere Alternative Code Signing" --keychain "$KC" "$APP"
  rm -f "$RUNNER_TEMP/sign.p12"
else
  for x in "$APP"/Contents/PlugIns/*.appex; do
    [ -e "$x" ] || continue
    codesign --force --sign - --entitlements packaging/macos/safari-extension.entitlements "$x"
  done
  for b in aa-host aa-viewer anywhere; do
    codesign --force --sign - --identifier "com.minutecreative.anywhere.$b" "$APP/Contents/MacOS/$b"
  done
  codesign --force --sign - "$APP"
fi

# Disk image with the usual "drag to Applications" layout.
STAGE="$OUT/dmg"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "Anywhere" -srcfolder "$STAGE" -ov -format UDZO "$OUT/Anywhere-$VERSION-mac.dmg"
rm -rf "$STAGE"
echo "built $OUT/Anywhere-$VERSION-mac.dmg"

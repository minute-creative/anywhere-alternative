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

# Ad-hoc signature: free, no Apple account. macOS asks once to allow it
# (System Settings → Privacy & Security → Open Anyway).
codesign --force --deep --sign - "$APP"

# Disk image with the usual "drag to Applications" layout.
STAGE="$OUT/dmg"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "Anywhere" -srcfolder "$STAGE" -ov -format UDZO "$OUT/Anywhere-$VERSION-mac.dmg"
rm -rf "$STAGE"
echo "built $OUT/Anywhere-$VERSION-mac.dmg"

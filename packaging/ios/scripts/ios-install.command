#!/bin/bash
# Installs the built Anywhere iPhone app on the iPhone connected by cable.
# Log: ~/anywhere-alternative/ios-install-log.txt
cd ~/anywhere-alternative || exit 1
exec > >(tee ios-install-log.txt) 2>&1
echo "== Anywhere iPhone install $(date) =="
DIR=$(xcodebuild -project packaging/ios/AnywhereiOS.xcodeproj -target Anywhere -configuration Release -sdk iphoneos \
  -showBuildSettings 2>/dev/null | awk -F' = ' '/ TARGET_BUILD_DIR = /{print $2; exit}')
APP="$DIR/Anywhere.app"
if [ ! -d "$APP" ]; then
  # Xcode may be set to build into DerivedData instead: take the newest.
  APP=$(ls -dt ~/Library/Developer/Xcode/DerivedData/*/Build/Products/Release-iphoneos/Anywhere.app \
        packaging/ios/build/Release-iphoneos/Anywhere.app 2>/dev/null | head -n1)
fi
echo "app: $APP"
[ -n "$APP" ] && [ -d "$APP" ] || { echo "== INSTALL FAILED: app not built yet"; exit 1; }
xcrun devicectl list devices 2>&1 | tee /tmp/aa-devices.txt
DEV=$(awk '/iPhone/ && /(available|connected)/ {for (i=1;i<=NF;i++) if ($i ~ /^[0-9A-F]{8}-[0-9A-F]{4}-/) {print $i; exit}}' /tmp/aa-devices.txt)
[ -n "$DEV" ] || { echo "== INSTALL FAILED: no connected iPhone found (cable in, phone unlocked?)"; exit 1; }
echo "device: $DEV"
if xcrun devicectl device install app --device "$DEV" "$APP"; then
  echo "== INSTALL OK =="
else
  echo "== INSTALL FAILED (see above) =="
fi
echo "You can close this window."

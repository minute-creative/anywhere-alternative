#!/bin/bash
# Turns the browser add-on into a Safari extension and puts it inside
# Anywhere.app (Contents/PlugIns). Safari lists extensions that come inside
# an app; there is no other way to install one.
#
#   packaging/macos/build-safari-ext.sh <path/to/Anywhere.app> <version>
#
# Needs Xcode (GitHub's Mac machines have it). Run after
# packaging/browser/build_addon.py made dist/addon/safari.
#
# The extension is not signed by Apple (that needs the paid developer
# account), so Safari only shows it while Develop → "Allow Unsigned
# Extensions" is ticked; Safari clears that tick each time it quits.
set -euo pipefail
APP="$1"
VERSION="$2"
WORK="$(mktemp -d)"
NAME="Anywhere Camera"

xcrun safari-web-extension-converter dist/addon/safari \
  --project-location "$WORK" --app-name "$NAME" \
  --bundle-identifier com.minutecreative.anywhere \
  --macos-only --swift --copy-resources --no-open --no-prompt --force

PROJ="$(find "$WORK" -name '*.xcodeproj' -maxdepth 3 | head -1)"
echo "Safari project: $PROJ"
xcodebuild -list -project "$PROJ"
TARGET="$(xcodebuild -list -project "$PROJ" | sed -n '/Targets:/,/^$/p' | grep -i 'extension' | head -1 | sed 's/^ *//')"
echo "building target: $TARGET"
xcodebuild -project "$PROJ" -target "$TARGET" -configuration Release \
  SYMROOT="$WORK/build" CODE_SIGNING_ALLOWED=NO CODE_SIGN_IDENTITY="" \
  MARKETING_VERSION="$VERSION" CURRENT_PROJECT_VERSION="$VERSION" \
  MACOSX_DEPLOYMENT_TARGET=13.0 build

APPEX="$(find "$WORK/build" -name '*.appex' -maxdepth 3 | head -1)"
[ -n "$APPEX" ] || { echo "no .appex was built"; exit 1; }
mkdir -p "$APP/Contents/PlugIns"
rm -rf "$APP/Contents/PlugIns/"*.appex
cp -R "$APPEX" "$APP/Contents/PlugIns/"
echo "added $(basename "$APPEX") to $APP"
rm -rf "$WORK"

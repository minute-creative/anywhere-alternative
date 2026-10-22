#!/bin/bash
# Rebuild the Anywhere iPhone app and install it on the iPhone (cable), in one go.
# Log: ~/anywhere-alternative/ios-update-log.txt
cd ~/anywhere-alternative || exit 1
exec > >(tee ios-update-log.txt) 2>&1
echo "== Anywhere iPhone update $(date) =="
packaging/ios/scripts/ios-build.command > /dev/null 2>&1
grep -E "error|exit:|BUILD (SUCCEEDED|FAILED)" ios-build-log.txt | head -40
grep -q "BUILD SUCCEEDED" ios-build-log.txt || { echo "== UPDATE FAILED at build =="; exit 1; }
packaging/ios/scripts/ios-install.command > /dev/null 2>&1
tail -3 ios-install-log.txt
grep -q "INSTALL OK" ios-install-log.txt && echo "== UPDATE OK ==" || echo "== UPDATE FAILED at install =="
echo "You can close this window."

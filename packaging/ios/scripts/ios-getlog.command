#!/bin/bash
# Copies Anywhere's viewer.log from the iPhone (cable) to ~/anywhere-alternative/ios-viewer.log
cd ~/anywhere-alternative || exit 1
exec > >(tee ios-getlog-log.txt) 2>&1
DEV=$(xcrun devicectl list devices 2>/dev/null | awk '/iPhone/ && /(available|connected)/ {for (i=1;i<=NF;i++) if ($i ~ /^[0-9A-F]{8}-[0-9A-F]{4}-/) {print $i; exit}}')
echo "device: $DEV"
rm -f ios-viewer.log
xcrun devicectl device copy from --device "$DEV" --domain-type appDataContainer \
  --domain-identifier com.minutecreative.anywhere.ios --source Documents/viewer.log --destination ios-viewer.log
ls -la ios-viewer.log && echo "== GOT LOG ==" || echo "== NO LOG =="

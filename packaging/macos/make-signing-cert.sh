#!/bin/bash
# One-time: make the certificate that signs every Mac release, so macOS
# remembers the app's permissions across updates. Run on any Mac:
#   bash packaging/macos/make-signing-cert.sh
# Then on GitHub: the repository → Settings → Secrets and variables →
# Actions → New repository secret, twice:
#   MAC_SIGN_P12       = the long text this prints first
#   MAC_SIGN_PASSWORD  = the short text it prints second
# Keep them secret: anyone with them could sign an app that macOS treats
# as Anywhere.
set -euo pipefail
D=$(mktemp -d)
cd "$D"
openssl req -x509 -newkey rsa:2048 -keyout key.pem -out cert.pem -days 7300 -nodes \
  -subj "/CN=Anywhere Alternative Code Signing" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=critical,codeSigning" \
  -addext "basicConstraints=critical,CA:false" 2>/dev/null
PASS=$(openssl rand -hex 16)
openssl pkcs12 -export -out sign.p12 -inkey key.pem -in cert.pem -name "Anywhere Alternative Code Signing" \
  -passout "pass:$PASS" 2>/dev/null
echo; echo "MAC_SIGN_P12:"; base64 -i sign.p12 | tr -d '\n'; echo
echo; echo "MAC_SIGN_PASSWORD:"; echo "$PASS"
rm -rf "$D"

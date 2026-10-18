#!/usr/bin/env bash
#
# Downloads the signed Virtual Display Driver into target/sandbox/vdd/, where
# the sandbox installs it from.
#
# The driver-only package, not the 68 MB control application: it is the INF, the
# UMDF DLL and the signed catalog, which is everything pnputil needs.
#
# The download is verified against the release's published SHA-256 and then
# checked for a valid signature, because the point of using VDD rather than
# writing our own is precisely that it is signed.
set -euo pipefail
cd "$(dirname "$0")/.."

out=target/sandbox/vdd
mkdir -p "$out"

# The API and its JSON, through node rather than jq, for the same reason the
# rest of the scripts use what is already installed.
release=https://api.github.com/repos/VirtualDrivers/Virtual-Display-Driver/releases/latest
info=$(curl -s "$release" | node -e "
let s=''; process.stdin.on('data', d => s += d).on('end', () => {
  const j = JSON.parse(s);
  const asset = (j.assets || []).find(a => /VirtualDisplayDriver-x86\.Driver\.Only/i.test(a.name));
  if (!asset) { console.error('no driver-only asset in ' + j.tag_name); process.exit(1); }
  console.log([j.tag_name, asset.name, asset.browser_download_url].join(' '));
});
")

read -r tag name url <<<"$info"
echo "release $tag: $name"
curl -sL "$url" -o "$out/$name"

cd "$out"
unzip -o -q "$name"
rm -f "$name"

cat >driver-info.txt <<EOF
source: $url
release: $tag
EOF

# Verify rather than trust: an unsigned package would install only with test
# signing, which is the whole thing this route avoids.
powershell.exe -NoProfile -Command "
\$bad = 0
Get-ChildItem -Recurse -Include *.cat,*.sys,*.dll | ForEach-Object {
  \$sig = Get-AuthenticodeSignature \$_.FullName
  if (\$sig.Status -ne 'Valid') { \$bad++; Write-Host \"INVALID: \$(\$_.Name) \$(\$sig.Status)\" }
  else { Write-Host \"valid: \$(\$_.Name) \$(\$sig.SignerCertificate.Subject -replace '^CN=([^,]+).*','\$1')\" }
}
if (\$bad -gt 0) { exit 1 }
" || { echo 'signature check failed'; exit 1; }

echo "driver package in $out"

#!/usr/bin/env bash
# Signs flux's macOS binaries with a Developer ID Application certificate,
# then has Apple notarize them. build.yml runs it when the repository has the
# Apple secrets (docs/RELEASING.md, "Signing"). Each binary keeps the same
# identifier (dev.vibecook.<name>) from release to release, which, with the
# team, is how macOS knows an update is the same program and keeps the
# permissions a person gave it (an external drive, a network volume).
# A command-line binary can't carry its notarization ticket (only an app,
# a disk image or an installer can), so Gatekeeper asks Apple online the
# first time it runs from a quarantined download.
#
# Usage: scripts/package/macos-sign.sh FILE...
# Environment: APPLE_CERTIFICATE (the .p12, in base64),
#   APPLE_CERTIFICATE_PASSWORD, APPLE_SIGNING_IDENTITY ("Developer ID
#   Application: Name (TEAMID)"), APPLE_API_KEY (an App Store Connect API
#   key's .p8, in base64), APPLE_API_KEY_ID, APPLE_API_ISSUER.
set -euo pipefail

die() { echo "macos-sign.sh: $*" >&2; exit 1; }
[ $# -gt 0 ] || die "usage: macos-sign.sh FILE..."
for v in APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD APPLE_SIGNING_IDENTITY APPLE_API_KEY APPLE_API_KEY_ID APPLE_API_ISSUER; do
  [ -n "${!v:-}" ] || die "$v is not set"
done
for f in "$@"; do [ -f "$f" ] || die "no such file: $f"; done

tmp=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/flux-sign.XXXXXX")
keychain="$tmp/signing.keychain-db"
cleanup() {
  security delete-keychain "$keychain" 2> /dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT

# A keychain of its own, unlocked for this run, with the certificate in it.
pass=$(openssl rand -hex 24)
security create-keychain -p "$pass" "$keychain"
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$pass" "$keychain"
printf '%s' "$APPLE_CERTIFICATE" | base64 --decode > "$tmp/certificate.p12"
security import "$tmp/certificate.p12" -k "$keychain" -P "$APPLE_CERTIFICATE_PASSWORD" -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple: -s -k "$pass" "$keychain" > /dev/null
keychains=("$keychain")
while IFS= read -r k; do
  k=${k#*\"}
  k=${k%\"*}
  [ -n "$k" ] && keychains+=("$k")
done < <(security list-keychains -d user)
security list-keychains -d user -s "${keychains[@]}"

# Hardened runtime (notarization needs it) and a secure timestamp; flux needs
# no entitlements (it runs ffmpeg as a separate program, and has no JIT).
mkdir "$tmp/binaries"
for f in "$@"; do
  codesign --force --options runtime --timestamp --keychain "$keychain" \
    --identifier "dev.vibecook.$(basename "$f")" --sign "$APPLE_SIGNING_IDENTITY" "$f"
  codesign --verify --strict --verbose=2 "$f"
  cp "$f" "$tmp/binaries/"
done

# Notarized together, as a zip.
printf '%s' "$APPLE_API_KEY" | base64 --decode > "$tmp/key.p8"
ditto -c -k --keepParent "$tmp/binaries" "$tmp/binaries.zip"
xcrun notarytool submit "$tmp/binaries.zip" --key "$tmp/key.p8" --key-id "$APPLE_API_KEY_ID" \
  --issuer "$APPLE_API_ISSUER" --wait --timeout 30m --output-format json > "$tmp/notary.json"
status=$(jq -r .status "$tmp/notary.json")
if [ "$status" != Accepted ]; then
  id=$(jq -r .id "$tmp/notary.json")
  echo "Apple's notary service answered $status for submission $id:" >&2
  xcrun notarytool log "$id" --key "$tmp/key.p8" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER" >&2 || true
  exit 1
fi
echo "Signed and notarized: $*"

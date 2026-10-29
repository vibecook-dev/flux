#!/usr/bin/env bash
# Signs flux's macOS code with a Developer ID Application certificate and
# has Apple notarize it. build.yml runs it when the repository has the Apple
# secrets (docs/RELEASING.md, "Signing"); each form is a step of its own:
#
#   macos-sign.sh FILE...    the release packages' binaries, signed, then
#                            notarized together
#   macos-sign.sh --app APP  flux.app: each program in it, then the bundle,
#                            which seals the rest (notarized with its disk
#                            image, next)
#   macos-sign.sh --dmg DMG  the disk image: signed, notarized, and the
#                            ticket stapled to it
#
# Each program keeps the same identifier from release to release
# (dev.vibecook.<name> in the packages, dev.vibecook.flux.<name> in the app,
# whose bundle is dev.vibecook.flux), which, with the team, is how macOS knows
# an update is the same program and keeps the permissions a person gave it
# (an external drive, a network volume). Hardened runtime, a secure
# timestamp, and no entitlements: flux runs ffmpeg and the sidecar as
# programs of their own, and has no JIT.
#
# A command-line binary can't carry its notarization ticket (only an app,
# a disk image or an installer can), so Gatekeeper asks Apple online the
# first time it runs from a quarantined download. The disk image carries its
# own, so flux.app opens offline too.
#
# Environment: APPLE_CERTIFICATE (the .p12, in base64),
#   APPLE_CERTIFICATE_PASSWORD, APPLE_SIGNING_IDENTITY ("Developer ID
#   Application: Name (TEAMID)"), APPLE_API_KEY (an App Store Connect API
#   key's .p8, in base64), APPLE_API_KEY_ID, APPLE_API_ISSUER.
set -euo pipefail

die() { echo "macos-sign.sh: $*" >&2; exit 1; }
mode=files
case "${1:-}" in
  --app) mode=app; shift ;;
  --dmg) mode=dmg; shift ;;
  "") die "usage: macos-sign.sh FILE... | --app APP | --dmg DMG" ;;
esac
[ $# -gt 0 ] || die "usage: macos-sign.sh FILE... | --app APP | --dmg DMG"
for v in APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD APPLE_SIGNING_IDENTITY APPLE_API_KEY APPLE_API_KEY_ID APPLE_API_ISSUER; do
  [ -n "${!v:-}" ] || die "$v is not set"
done
case "$mode" in
  files) for f in "$@"; do [ -f "$f" ] || die "no such file: $f"; done ;;
  app) if [ $# -ne 1 ] || [ ! -f "$1/Contents/Info.plist" ]; then die "--app: not an app bundle: $*"; fi ;;
  dmg) if [ $# -ne 1 ] || [ ! -f "$1" ]; then die "--dmg: no such disk image: $*"; fi ;;
esac

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

# `sign IDENTIFIER PATH`: hardened runtime and a secure timestamp.
sign() {
  codesign --force --options runtime --timestamp --keychain "$keychain" \
    --identifier "$1" --sign "$APPLE_SIGNING_IDENTITY" "$2"
}

# `notarize FILE`: Apple's notary service checks FILE (a zip or a disk
# image) and everything in it, or says why not.
printf '%s' "$APPLE_API_KEY" | base64 --decode > "$tmp/key.p8"
notarize() {
  xcrun notarytool submit "$1" --key "$tmp/key.p8" --key-id "$APPLE_API_KEY_ID" \
    --issuer "$APPLE_API_ISSUER" --wait --timeout 30m --output-format json > "$tmp/notary.json"
  local status id
  status=$(jq -r .status "$tmp/notary.json")
  if [ "$status" != Accepted ]; then
    id=$(jq -r .id "$tmp/notary.json")
    echo "Apple's notary service answered $status for submission $id:" >&2
    xcrun notarytool log "$id" --key "$tmp/key.p8" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER" >&2 || true
    exit 1
  fi
}

case "$mode" in
  files)
    mkdir "$tmp/binaries"
    for f in "$@"; do
      sign "dev.vibecook.$(basename "$f")" "$f"
      codesign --verify --strict --verbose=2 "$f"
      cp "$f" "$tmp/binaries/"
    done
    # Notarized together, as a zip.
    ditto -c -k --keepParent "$tmp/binaries" "$tmp/binaries.zip"
    notarize "$tmp/binaries.zip"
    echo "Signed and notarized: $*"
    ;;
  app)
    app=$1
    # Inside out: every program in Contents/Helpers (FFmpeg's in ffmpeg/
    # too), then the bundle, whose signature covers its executable and seals
    # the rest.
    while IFS= read -r -d '' f; do
      if file -b "$f" | grep -q 'Mach-O'; then
        sign "dev.vibecook.flux.$(basename "$f")" "$f"
      fi
    done < <(find "$app/Contents/Helpers" -type f -print0 | sort -z)
    id=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app/Contents/Info.plist")
    sign "$id" "$app"
    codesign --verify --deep --strict --verbose=2 "$app"
    echo "Signed: $app"
    ;;
  dmg)
    dmg=$1
    codesign --force --timestamp --keychain "$keychain" --sign "$APPLE_SIGNING_IDENTITY" "$dmg"
    codesign --verify --strict --verbose=2 "$dmg"
    notarize "$dmg"
    xcrun stapler staple "$dmg"
    xcrun stapler validate "$dmg"
    echo "Signed, notarized and stapled: $dmg"
    ;;
esac

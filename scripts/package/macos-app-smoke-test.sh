#!/usr/bin/env bash
# Installs flux's macOS app from its disk image on this Mac (a CI runner: it
# changes the machine and the person's ~/Library) and checks it:
#
# - The disk image: signed, notarized and stapled when the app is signed
#   with a Developer ID; flux.app beside a link to /Applications, with the
#   window's layout.
# - flux.app, copied to /Applications as a person drags it: arm64 and x86_64
#   throughout, the version, sealed (and accepted by Gatekeeper when
#   signed), marked "app", with the web client and FFmpeg; `flux-server
#   --check` from a terminal finds the bundled FFmpeg (with tonemapx) and the
#   settings file the app writes.
# - Opened, the menu bar app starts flux-server, which answers on port 8096
#   as the version, with its log in ~/Library/Logs/flux/flux.log.
# - flux updates itself, to the same disk image taken for a newer version
#   (FLUX_UPDATE_CURRENT): it finds it in a stand-in for GitHub's releases
#   (with SHA256SUMS), downloads it, puts its flux.app in place of the
#   running one (a new bundle in /Applications, nothing left beside it), and
#   the app starts itself again, as a new process, with a new flux-server
#   that answers with no update error.
# - Quitting the app stops flux-server.
#
# Usage: scripts/package/macos-app-smoke-test.sh --dmg dist/flux-<version>-universal-apple-darwin.dmg --version <version>
set -euo pipefail

dmg="" version=""
die() { echo "macos-app-smoke-test.sh: $*" >&2; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --dmg) dmg=${2:?}; shift 2 ;;
    --version) version=${2:?}; shift 2 ;;
    -h | --help) sed -n '2,24p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done
[ -f "$dmg" ] || die "--dmg: no such file: ${dmg:-<unset>}"
[ -n "$version" ] || die "--version is required"
dmg=$(cd "$(dirname "$dmg")" && pwd)/$(basename "$dmg")

app=/Applications/flux.app
data="$HOME/Library/Application Support/flux"
log="$HOME/Library/Logs/flux/flux.log"
base=http://127.0.0.1:8096
work=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/flux-app-smoke.XXXXXX")
mnt="$work/mnt"
site_pid=""
cleanup() {
  [ -z "$site_pid" ] || kill "$site_pid" 2> /dev/null || true
  hdiutil detach "$mnt" -force > /dev/null 2>&1 || true
}
trap cleanup EXIT

show_log() {
  if [ -f "$log" ]; then
    echo "--- $log (last 60 lines)"
    tail -n 60 "$log"
  fi
}
fail() {
  show_log
  die "$*"
}

# `version_at`: the version answering on port 8096, if any.
version_at() {
  curl -fsS --max-time 3 "$base/api/v1/server" 2> /dev/null | jq -r .version 2> /dev/null || true
}
wait_answers() {
  local what=$1 i
  for ((i = 0; i < 120; i++)); do
    if [ "$(version_at)" = "$version" ]; then return 0; fi
    sleep 0.5
  done
  fail "$what: flux $version didn't answer on port 8096 within a minute"
}
# The PIDs of the menu bar app and of flux-server from /Applications.
app_pid() { pgrep -f "^$app/Contents/MacOS/flux" | head -n 1 || true; }
server_pid() { pgrep -f "^$app/Contents/Helpers/flux-server" | head -n 1 || true; }

echo "== the disk image"
signed=false
if codesign -dv "$dmg" 2>&1 | grep -q '^TeamIdentifier=[A-Z0-9]'; then
  signed=true
  codesign --verify --strict --verbose=2 "$dmg"
  xcrun stapler validate "$dmg"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg"
else
  echo "(not signed with a Developer ID: no Gatekeeper checks)"
fi
mkdir -p "$mnt"
hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$mnt" "$dmg" > /dev/null
ls -la "$mnt"
[ -d "$mnt/flux.app" ] || die "the disk image has no flux.app"
[ "$(readlink "$mnt/Applications")" = /Applications ] || die "the disk image's Applications isn't a link to /Applications"
[ -f "$mnt/.DS_Store" ] || die "the disk image has no window layout (.DS_Store)"
ls "$mnt"/.background* > /dev/null 2>&1 || die "the disk image has no background"

echo "== flux.app, dragged to Applications"
rm -rf "$app"
ditto "$mnt/flux.app" "$app"
hdiutil detach "$mnt" > /dev/null
plist() { /usr/libexec/PlistBuddy -c "Print :$1" "$app/Contents/Info.plist"; }
[ "$(plist CFBundleIdentifier)" = dev.vibecook.flux ] || die "the bundle identifier is $(plist CFBundleIdentifier)"
[ "$(plist CFBundleShortVersionString)" = "$version" ] || die "flux.app says version $(plist CFBundleShortVersionString)"
[ "$(plist LSUIElement)" = true ] || die "flux.app isn't a menu bar app (LSUIElement)"
[ "$(tr -d '\n' < "$app/Contents/Resources/install-kind")" = app ] || die "install-kind isn't app"
for f in MacOS/flux Helpers/flux-server Helpers/flux Helpers/ffmpeg/ffmpeg Helpers/ffmpeg/ffprobe \
  Resources/web/index.html Resources/flux.toml Resources/flux.icns Resources/StatusIcon.png \
  Resources/THIRD_PARTY_LICENSES Resources/ffmpeg/LICENSE.txt Resources/ffmpeg/SOURCE.txt; do
  [ -e "$app/Contents/$f" ] || die "flux.app has no Contents/$f"
done
while IFS= read -r -d '' f; do
  file -b "$f" | grep -q 'Mach-O' || continue
  archs=$(lipo -archs "$f")
  case " $archs " in *" arm64 "*" x86_64 "* | *" x86_64 "*" arm64 "*) ;; *) die "${f#"$app/"} is $archs only" ;; esac
done < <(find "$app/Contents/MacOS" "$app/Contents/Helpers" -type f -print0)
codesign --verify --deep --strict --verbose=2 "$app"
if [ "$signed" = true ]; then
  spctl --assess --type execute --verbose=2 "$app"
fi

echo "== flux-server --check, from a terminal"
# The app's settings file is written when the app first starts; this one
# shows that a terminal's flux-server finds it, with its library.
mkdir -p "$data" "$work/media"
{
  cat "$app/Contents/Resources/flux.toml"
  printf '\n[[library]]\nname = "Smoke"\nkind = "videos"\npath = "%s"\n' "$work/media"
} > "$data/flux.toml"
report=$("$app/Contents/Helpers/flux-server" --check) || {
  echo "$report"
  die "flux-server --check failed"
}
echo "$report"
for want in "$app/Contents/Helpers/ffmpeg/ffmpeg" "$app/Contents/Helpers/ffmpeg/ffprobe" tonemapx "library  Smoke (videos): $work/media"; do
  grep -qF "$want" <<< "$report" || die "flux-server --check doesn't say: $want"
done

echo "== the app starts flux"
# The browser isn't opened for Set up flux on a CI runner.
defaults write dev.vibecook.flux OpenedFluxOnce -bool true
# The stand-in for GitHub's releases is read from the first start on, and
# flux takes itself for an older version (the same as the update test's).
site="$work/release"
mkdir -p "$site"
name=$(basename "$dmg")
cp "$dmg" "$site/$name"
(cd "$site" && shasum -a 256 "$name" > SHA256SUMS)
site_url=http://127.0.0.1:18099
jq -n --arg v "$version" --arg name "$name" --arg base "$site_url" \
  --argjson size "$(stat -f %z "$site/$name")" --argjson sums "$(stat -f %z "$site/SHA256SUMS")" \
  '[{tag_name: ("v" + $v), draft: false, prerelease: true, html_url: ($base + "/release"),
     published_at: "2026-01-01T00:00:00Z", body: "* the smoke test'"'"'s release",
     assets: [{name: $name, browser_download_url: ($base + "/" + $name), size: $size},
              {name: "SHA256SUMS", browser_download_url: ($base + "/SHA256SUMS"), size: $sums}]}]' \
  > "$site/releases.json"
python3 -m http.server 18099 --bind 127.0.0.1 --directory "$site" > "$work/site.log" 2>&1 &
site_pid=$!
for ((i = 0; i < 60; i++)); do
  curl -fsS --max-time 2 "$site_url/releases.json" > /dev/null 2>&1 && break
  sleep 0.5
done
FLUX_UPDATE_SOURCE="$site_url/releases.json" FLUX_UPDATE_CURRENT=0.0.1-alpha.1 \
  "$app/Contents/MacOS/flux" > "$work/app.log" 2>&1 &
wait_answers "after opening flux.app"
first_app=$(app_pid)
first_server=$(server_pid)
[ -n "$first_app" ] && [ -n "$first_server" ] || fail "the app ($first_app) or flux-server ($first_server) isn't running"
[ "$(ps -o ppid= -p "$first_server" | tr -d ' ')" = "$first_app" ] || fail "flux-server isn't the app's"
grep -q 'starting' "$log" || fail "flux-server logs nowhere near $log"
echo "flux $version answers: the app is $first_app, flux-server $first_server"

echo "== flux updates itself"
inode=$(stat -f %i "$app")
u=$(curl -fsS -X POST -H 'X-Flux-Request: 1' "$base/api/v1/admin/update/apply") || fail "update/apply was refused"
echo "update: $(jq -r '.phase + ", from " + .current' <<< "$u")"
for ((i = 0; i < 600; i++)); do
  now=$(app_pid)
  if [ -n "$now" ] && [ "$now" != "$first_app" ] && [ "$(version_at)" = "$version" ]; then break; fi
  sleep 1
done
[ -n "$now" ] && [ "$now" != "$first_app" ] || fail "flux.app didn't start again as a new process within 10 minutes"
[ "$(stat -f %i "$app")" != "$inode" ] || fail "$app is the same bundle: the update didn't replace it"
if ls -d /Applications/.flux-* > /dev/null 2>&1; then fail "left beside flux.app: $(ls -d /Applications/.flux-*)"; fi
wait_answers "after updating itself"
u=$(curl -fsS -H 'X-Flux-Request: 1' "$base/api/v1/admin/update")
echo "flux $(jq -r .current <<< "$u") answers, $(jq -r .phase <<< "$u")"
# Started by `open`, without the stand-in or the pretence.
[ "$(jq -r .current <<< "$u")" = "$version" ] && [ "$(jq -r .phase <<< "$u")" = idle ] && [ "$(jq -r '.error // ""' <<< "$u")" = "" ] ||
  fail "after the update: $u"
codesign --verify --deep --strict "$app"
grep -q 'name = "Smoke"' "$data/flux.toml" || fail "the update replaced $data/flux.toml"

echo "== quitting the app stops flux"
server=$(server_pid)
osascript -e 'tell application id "dev.vibecook.flux" to quit'
for ((i = 0; i < 80; i++)); do
  if [ -z "$(app_pid)" ] && ! kill -0 "$server" 2> /dev/null; then break; fi
  sleep 0.5
done
[ -z "$(app_pid)" ] || fail "flux.app is still running"
! kill -0 "$server" 2> /dev/null || fail "flux-server ($server) outlived the app"
grep -q 'flux stopped' "$log" || fail "flux-server didn't stop gracefully"
[ ! -e "$data/server.json" ] || fail "server.json is still there"
echo "The disk image installs flux.app, which runs flux $version, updates itself and quits."

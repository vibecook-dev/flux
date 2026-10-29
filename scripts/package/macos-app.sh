#!/usr/bin/env bash
# Builds flux's macOS app and its disk image,
# flux-<version>-universal-apple-darwin.dmg, in two steps, so that the app
# can be signed in between (build.yml):
#
#   --step bundle  builds flux.app in --stage from the two macOS release
#                  packages (package.sh's), their flux-server, flux and
#                  sidecar made universal (arm64 and x86_64) with lipo; the
#                  menu bar app (crates/flux-app) built for each; and the
#                  FFmpeg that ffmpeg-macos.env pins (downloaded, checked
#                  against its SHA-256, made universal), with its licence and
#                  where its source is. Signed ad hoc, so the bundle checks
#                  out as a whole wherever it is copied; a Developer ID
#                  signature replaces that (macos-sign.sh --app).
#   --step dmg     makes the disk image of --stage/flux.app in --out with
#                  dmgbuild (pinned in macos/dmgbuild-requirements.txt):
#                  flux.app beside a link to Applications.
#
#   flux.app/Contents/
#     Info.plist                  (macos/Info.plist, with the version)
#     MacOS/flux                  the menu bar app
#     Helpers/flux-server         flux-server finds the web client at
#     Helpers/flux                ../Resources/web, the sidecar beside it and
#     Helpers/sidecar-slim        FFmpeg in ffmpeg/ (with the sidecar, when
#     Helpers/ffmpeg/ffmpeg       the packages have it)
#     Helpers/ffmpeg/ffprobe
#     Resources/web/              the web client
#     Resources/install-kind      "app": how flux updates itself
#                                 (crates/flux-server/src/update/app.rs)
#     Resources/flux.toml         the settings file's template
#     Resources/flux.icns  StatusIcon.png  StatusIcon@2x.png
#     Resources/LICENSE  README.md  THIRD_PARTY_LICENSES
#     Resources/ffmpeg/LICENSE.txt  SOURCE.txt
#
# Usage:
#   scripts/package/macos-app.sh --step bundle --version 0.1.0 --stage DIR \
#     --arm64 dist/flux-0.1.0-aarch64-apple-darwin.tar.gz \
#     --x86_64 dist/flux-0.1.0-x86_64-apple-darwin.tar.gz \
#     --app-arm64 target/aarch64-apple-darwin/release/flux-app \
#     --app-x86_64 target/x86_64-apple-darwin/release/flux-app \
#     [--app-licenses FILE]
#   scripts/package/macos-app.sh --step dmg --version 0.1.0 --stage DIR --out dist
#
# --app-licenses: the menu bar app's crates and their licences
# (cargo-about with macos/about-app.hbs), added to THIRD_PARTY_LICENSES.
# Runs on macOS (lipo, codesign, hdiutil); the dmg step needs Python 3.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
step="" version="" stage="" out="" arm64="" x86_64="" app_arm64="" app_x86_64="" app_licenses=""
die() { echo "macos-app.sh: $*" >&2; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --step) step=${2:?}; shift 2 ;;
    --version) version=${2:?}; shift 2 ;;
    --stage) stage=${2:?}; shift 2 ;;
    --out) out=${2:?}; shift 2 ;;
    --arm64) arm64=${2:?}; shift 2 ;;
    --x86_64) x86_64=${2:?}; shift 2 ;;
    --app-arm64) app_arm64=${2:?}; shift 2 ;;
    --app-x86_64) app_x86_64=${2:?}; shift 2 ;;
    --app-licenses) app_licenses=${2:?}; shift 2 ;;
    -h | --help) sed -n '2,46p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done
[ -n "$version" ] || die "--version is required"
[ -n "$stage" ] || die "--stage is required"
case "$version" in v*) die "--version takes the bare version (0.1.0), not the tag" ;; esac
[ "$(uname -s)" = Darwin ] || die "builds on macOS only"
app="$stage/flux.app"

# The minimum macOS: the sidecar's (and SMAppService's, for Open at Login).
# Keep it the one INSTALL.md and RELEASING.md give for the app.
MINIMUM_SYSTEM=13.0

pin() {
  local v
  v=$(sed -n "s/^$1=//p" "$here/ffmpeg-macos.env")
  [ -n "$v" ] || die "$1 is not set in ffmpeg-macos.env"
  printf '%s' "$v"
}

# version_gt A B: whether the dotted version A is above B (smoke-test.sh's).
version_gt() {
  awk -v a="$1" -v b="$2" 'BEGIN {
    n = split(a, x, "."); m = split(b, y, "."); if (m > n) n = m
    for (i = 1; i <= n; i++) {
      if (x[i] + 0 > y[i] + 0) exit 0
      if (x[i] + 0 < y[i] + 0) exit 1
    }
    exit 1
  }'
}

# `universal OUT IN...`: the arm64 and x86_64 binaries as one.
universal() {
  local dest=$1
  shift
  lipo -create -output "$dest" "$@"
  chmod 0755 "$dest"
  lipo "$dest" -verify_arch arm64 x86_64 || die "$dest isn't arm64 and x86_64"
}

bundle() {
  for f in "$arm64" "$x86_64" "$app_arm64" "$app_x86_64"; do
    [ -f "$f" ] || die "missing input: ${f:-<unset>} (see --help)"
  done
  if [ -n "$app_licenses" ]; then [ -s "$app_licenses" ] || die "--app-licenses: empty or missing: $app_licenses"; fi
  local work
  work=$(mktemp -d "${TMPDIR:-/tmp}/flux-app.XXXXXX")
  # shellcheck disable=SC2064
  trap "rm -rf '$work'" EXIT

  # The two packages, each one folder.
  local a x
  for arch in arm64 x86_64; do
    mkdir -p "$work/$arch"
    tar -xzf "${!arch}" -C "$work/$arch"
  done
  a=$(find "$work/arm64" -mindepth 1 -maxdepth 1 -type d)
  x=$(find "$work/x86_64" -mindepth 1 -maxdepth 1 -type d)
  for top in "$a" "$x"; do
    for f in bin/flux-server bin/flux share/flux/web/index.html LICENSE README.md THIRD_PARTY_LICENSES; do
      [ -e "$top/$f" ] || die "$(basename "$top") has no $f"
    done
  done
  local sidecars=0
  [ -f "$a/bin/sidecar-slim" ] && sidecars=$((sidecars + 1))
  [ -f "$x/bin/sidecar-slim" ] && sidecars=$((sidecars + 1))
  [ "$sidecars" != 1 ] || die "only one of the packages has the sidecar"

  rm -rf "$app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Helpers/ffmpeg" "$app/Contents/Resources/ffmpeg"
  universal "$app/Contents/MacOS/flux" "$app_arm64" "$app_x86_64"
  universal "$app/Contents/Helpers/flux-server" "$a/bin/flux-server" "$x/bin/flux-server"
  universal "$app/Contents/Helpers/flux" "$a/bin/flux" "$x/bin/flux"
  if [ "$sidecars" = 2 ]; then
    universal "$app/Contents/Helpers/sidecar-slim" "$a/bin/sidecar-slim" "$x/bin/sidecar-slim"
  fi

  # FFmpeg: jellyfin-ffmpeg's two builds, checked, then one universal pair.
  local ffv
  ffv=$(pin JELLYFIN_FFMPEG_VERSION)
  for arch in ARM64 X86_64; do
    local asset sha
    asset=$(pin "${arch}_ASSET")
    sha=$(pin "${arch}_SHA256")
    echo "FFmpeg: $asset"
    curl -fsSL --retry 3 --proto '=https' -o "$work/$asset" \
      "https://github.com/jellyfin/jellyfin-ffmpeg/releases/download/v$ffv/$asset"
    (cd "$work" && echo "$sha  $asset" | shasum -a 256 -c -) || die "$asset: not the SHA-256 ffmpeg-macos.env pins"
    mkdir -p "$work/ffmpeg-$arch"
    tar -xJf "$work/$asset" -C "$work/ffmpeg-$arch"
  done
  for tool in ffmpeg ffprobe; do
    universal "$app/Contents/Helpers/ffmpeg/$tool" "$work/ffmpeg-ARM64/$tool" "$work/ffmpeg-X86_64/$tool"
  done
  cp "$here/macos/FFMPEG-LICENSE.txt" "$app/Contents/Resources/ffmpeg/LICENSE.txt"
  cat > "$app/Contents/Resources/ffmpeg/SOURCE.txt" << EOF
Contents/Helpers/ffmpeg holds FFmpeg from jellyfin-ffmpeg $ffv: Jellyfin's
portable macOS builds ($(pin ARM64_ASSET) and
$(pin X86_64_ASSET)), made into one universal ffmpeg and
one ffprobe, licensed under the GNU General Public License version 3:
LICENSE.txt.

Its complete source, with every library compiled into it and the scripts that
built it, is attached to the flux release this app comes from, as
jellyfin-ffmpeg-$ffv-macos-source.tar:
https://github.com/vibecook-dev/flux/releases/tag/v$version

flux runs ffmpeg and ffprobe as separate programs and links none of FFmpeg's
libraries.
EOF

  # Resources.
  local res="$app/Contents/Resources"
  cp -R "$a/share/flux/web" "$res/web"
  printf 'app\n' > "$res/install-kind"
  cp "$here/macos/flux.toml" "$here/macos/flux.icns" "$here/macos/StatusIcon.png" "$here/macos/StatusIcon@2x.png" "$res/"
  cp "$a/LICENSE" "$a/README.md" "$res/"
  {
    cat "$a/THIRD_PARTY_LICENSES"
    printf '\n\nflux.app also bundles FFmpeg (Contents/Helpers/ffmpeg), under the GPL:\nContents/Resources/ffmpeg/LICENSE.txt, and SOURCE.txt for its source.\n'
    if [ -n "$app_licenses" ]; then
      printf '\n'
      cat "$app_licenses"
    fi
  } > "$res/THIRD_PARTY_LICENSES"

  # Info.plist: the version as flux has it (shown in About), and as macOS
  # compares them (three numbers).
  local release=${version%%-*}
  [[ "$release" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "--version $version isn't a release version"
  sed -e "s/@VERSION@/$version/" -e "s/@BUNDLE_VERSION@/$release/" -e "s/@MINIMUM_SYSTEM@/$MINIMUM_SYSTEM/" \
    "$here/macos/Info.plist" > "$app/Contents/Info.plist"
  plutil -lint "$app/Contents/Info.plist"
  ! grep -q '@[A-Z_]*@' "$app/Contents/Info.plist" || die "Info.plist has a value left to fill in"

  # Each program runs on the minimum macOS: no LC_BUILD_VERSION minos (one
  # per architecture) above it.
  while IFS= read -r -d '' f; do
    file -b "$f" | grep -q 'Mach-O' || continue
    local minos found=""
    while IFS= read -r minos; do
      found=1
      if version_gt "$minos" "$MINIMUM_SYSTEM"; then
        die "${f#"$app/"} needs macOS $minos, more than the app's $MINIMUM_SYSTEM"
      fi
    done < <(otool -arch all -l "$f" | awk '$1 == "minos" { print $2 }')
    [ -n "$found" ] || die "can't read the minimum macOS of ${f#"$app/"}"
  done < <(find "$app/Contents/MacOS" "$app/Contents/Helpers" -type f -print0)

  find "$app" -type d -exec chmod 0755 {} +
  find "$app" -type f ! -perm -u+x -exec chmod 0644 {} +
  # Ad hoc: a seal over the whole bundle, until a Developer ID signs it.
  while IFS= read -r -d '' f; do
    file -b "$f" | grep -q 'Mach-O' && codesign --force --sign - --identifier "dev.vibecook.flux.$(basename "$f")" "$f"
  done < <(find "$app/Contents/Helpers" -type f -print0)
  codesign --force --sign - "$app"
  codesign --verify --deep --strict "$app"
  du -sh "$app"
  find "$app" -maxdepth 3 ! -path '*/Resources/web/*' | sort
}

dmg() {
  [ -n "$out" ] || die "--out is required"
  [ -d "$app" ] || die "no $app (run --step bundle first)"
  mkdir -p "$out"
  local dmg venv
  dmg="$(cd "$out" && pwd)/flux-$version-universal-apple-darwin.dmg"
  venv="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/flux-dmgbuild"
  if [ ! -x "$venv/bin/dmgbuild" ]; then
    python3 -m venv "$venv"
    "$venv/bin/pip" install --quiet --disable-pip-version-check --only-binary :all: --require-hashes \
      -r "$here/macos/dmgbuild-requirements.txt"
  fi
  rm -f "$dmg"
  "$venv/bin/dmgbuild" -s "$here/macos/dmg-settings.py" \
    -D app="$app" -D icon="$here/macos/flux.icns" -D background="$here/macos/dmg-background.png" \
    "flux $version" "$dmg"
  hdiutil verify "$dmg"
  ls -l "$dmg"
}

case "$step" in
  bundle) bundle ;;
  dmg) dmg ;;
  *) die "--step is bundle or dmg" ;;
esac

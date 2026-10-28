#!/usr/bin/env bash
# Collects the complete source of the FFmpeg the Windows installer ships
# (ffmpeg-windows.env) into one tar, ffmpeg-<version>-windows-source.tar,
# which every release attaches: whoever passes a GPL program on offers its
# source, and BtbN publishes binaries only. It holds:
#
#   FFmpeg-<commit>.tar.xz             FFmpeg at the commit the build is of
#   FFmpeg-Builds-<commit>.tar.xz      BtbN's scripts and patches that built it
#   libraries/<step>.tar.xz            every library those scripts compile in, at
#                                      its pinned commit: what BtbN's own
#                                      download.sh fetches (all of its steps)
#   README.txt                         what is where
#
# download.sh runs in BtbN's public base image, so this needs Docker (GitHub's
# Ubuntu runners have it).
#
# Usage: scripts/package/ffmpeg-windows-source.sh --out DIR
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
out=""
die() { echo "ffmpeg-windows-source.sh: $*" >&2; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --out) out=${2:?}; shift 2 ;;
    -h | --help) sed -n '2,19p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done
[ -n "$out" ] || die "usage: ffmpeg-windows-source.sh --out DIR"
mkdir -p "$out"
out=$(cd "$out" && pwd)

# The pins, read rather than sourced (windows-msi.ps1 reads them too).
pin() {
  local v
  v=$(sed -n "s/^$1=//p" "$here/ffmpeg-windows.env")
  [ -n "$v" ] || die "$1 is not set in ffmpeg-windows.env"
  printf '%s' "$v"
}
BTBN_TAG=$(pin BTBN_TAG)
BTBN_COMMIT=$(pin BTBN_COMMIT)
BTBN_ASSET=$(pin BTBN_ASSET)
FFMPEG_VERSION=$(pin FFMPEG_VERSION)
FFMPEG_COMMIT=$(pin FFMPEG_COMMIT)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
name="ffmpeg-$FFMPEG_VERSION-windows-source"
mkdir -p "$work/$name/libraries"

echo "BtbN/FFmpeg-Builds at $BTBN_COMMIT ($BTBN_TAG)"
git clone --quiet --filter=blob:none --no-checkout https://github.com/BtbN/FFmpeg-Builds.git "$work/FFmpeg-Builds"
git -C "$work/FFmpeg-Builds" checkout --quiet "$BTBN_COMMIT"
git -C "$work/FFmpeg-Builds" archive --format=tar --prefix=FFmpeg-Builds/ HEAD |
  xz -T0 > "$work/$name/FFmpeg-Builds-$BTBN_COMMIT.tar.xz"

echo "The libraries (BtbN's download.sh)"
# It runs in BtbN's base image, which it names after GITHUB_REPOSITORY when
# that is set (as in any GitHub workflow): name BtbN's.
(cd "$work/FFmpeg-Builds" && GITHUB_REPOSITORY=btbn/ffmpeg-builds ./download.sh)
# download.sh keeps <step>_<hash>.tar.xz, linked from <step>.tar.xz.
found=0
for f in "$work/FFmpeg-Builds/.cache/downloads/"*.tar.xz; do
  if [ -L "$f" ]; then
    cp -L "$f" "$work/$name/libraries/$(basename "$f")"
    found=$((found + 1))
  fi
done
[ "$found" -gt 0 ] || die "download.sh fetched nothing"

echo "FFmpeg at $FFMPEG_COMMIT"
git clone --quiet --filter=blob:none --no-checkout https://github.com/FFmpeg/FFmpeg.git "$work/FFmpeg"
git -C "$work/FFmpeg" checkout --quiet "$FFMPEG_COMMIT"
full=$(git -C "$work/FFmpeg" rev-parse HEAD)
git -C "$work/FFmpeg" archive --format=tar --prefix=FFmpeg/ HEAD | xz -T0 > "$work/$name/FFmpeg-$full.tar.xz"

cat > "$work/$name/README.txt" << EOF
The source of the FFmpeg in flux's Windows installer (ffmpeg\\ in its folder):
FFmpeg $FFMPEG_VERSION, built by BtbN/FFmpeg-Builds ($BTBN_TAG,
$BTBN_ASSET), licensed under the GNU GPL version 3.

FFmpeg-$full.tar.xz
    FFmpeg's source at the commit the build is of.
FFmpeg-Builds-$BTBN_COMMIT.tar.xz
    The scripts and patches that built it (https://github.com/BtbN/FFmpeg-Builds
    at $BTBN_COMMIT): scripts.d/ has one script per library, with the
    repository and commit it is built from and how it is configured.
libraries/
    Each of those libraries at that commit ($found archives), as the build's own
    download.sh fetched them.

To build it again, see FFmpeg-Builds/README.md: ./build.sh win64 gpl-shared 9.0.
flux runs ffmpeg.exe and ffprobe.exe as separate programs and links none of
FFmpeg's libraries.
EOF

tar -C "$work" -cf "$out/$name.tar" "$name"
ls -l "$out/$name.tar"

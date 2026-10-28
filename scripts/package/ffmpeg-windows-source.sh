#!/usr/bin/env bash
# Collects the complete source of the FFmpeg the Windows installer ships
# (ffmpeg-windows.env) into one tar, ffmpeg-<version>-windows-source.tar,
# which every release attaches: whoever passes a GPL program on offers its
# source, and BtbN publishes binaries only. It holds:
#
#   FFmpeg-<commit>.tar.xz          FFmpeg at the commit the build is of
#   FFmpeg-Builds-<commit>.tar.xz   BtbN's scripts and patches that built it
#   Dockerfile                      that build, as BtbN's generate.sh writes it
#                                   for it (BTBN_BUILD): its stages, in order
#   libraries/<stage>_<hash>.tar.xz the source of each library the build
#                                   compiles in, at its pinned commit, as
#                                   BtbN's download.sh fetches and caches it
#   README.txt                      what is where
#
# GitHub takes release assets under 2 GiB, and the libraries come to more,
# so they are spread over parts as needed: ffmpeg-<version>-windows-source-2.tar
# and on, with the same top folder, which unpack into one libraries/. Only
# the build's own libraries: download.sh fetches every variant's (Linux,
# non-free). It runs in BtbN's public base image, so this needs Docker
# (GitHub's Ubuntu runners have it).
#
# Usage: scripts/package/ffmpeg-windows-source.sh --out DIR
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
out=""
die() { echo "ffmpeg-windows-source.sh: $*" >&2; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --out) out=${2:?}; shift 2 ;;
    -h | --help) sed -n '2,22p' "$0"; exit 0 ;;
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
BTBN_BUILD=$(pin BTBN_BUILD)
FFMPEG_VERSION=$(pin FFMPEG_VERSION)
FFMPEG_COMMIT=$(pin FFMPEG_COMMIT)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
name="ffmpeg-$FFMPEG_VERSION-windows-source"
mkdir -p "$work/$name/libraries"
builds="$work/FFmpeg-Builds"

echo "BtbN/FFmpeg-Builds at $BTBN_COMMIT ($BTBN_TAG)"
git clone --quiet --filter=blob:none --no-checkout https://github.com/BtbN/FFmpeg-Builds.git "$builds"
git -C "$builds" checkout --quiet "$BTBN_COMMIT"
git -C "$builds" archive --format=tar --prefix=FFmpeg-Builds/ HEAD |
  xz -T0 > "$work/$name/FFmpeg-Builds-$BTBN_COMMIT.tar.xz"

# BtbN's scripts name their Docker images after GITHUB_REPOSITORY when it is
# set (as in any GitHub workflow): name BtbN's.
export GITHUB_REPOSITORY=btbn/ffmpeg-builds

echo "The build ($BTBN_BUILD): BtbN's generate.sh"
read -r -a build_args <<< "$BTBN_BUILD"
(cd "$builds" && ./generate.sh "${build_args[@]}" > /dev/null)
cp "$builds/Dockerfile" "$work/$name/Dockerfile"
# Each stage that compiles a library mounts its download:
# --mount=src=.cache/downloads/<stage>_<hash>.tar.xz
mapfile -t used < <(grep -o '\.cache/downloads/[^,]*\.tar\.xz' "$builds/Dockerfile" | sort -u)
[ "${#used[@]}" -gt 0 ] || die "the build's Dockerfile names no downloads"

echo "The libraries: BtbN's download.sh"
(cd "$builds" && ./download.sh)
for f in "${used[@]}"; do
  [ -f "$builds/$f" ] || die "download.sh fetched no $f, which the build uses"
  cp "$builds/$f" "$work/$name/libraries/"
done
echo "${#used[@]} libraries; the largest, in MB:"
(cd "$work/$name/libraries" && du -m -- * | sort -rn | head -15)

echo "FFmpeg at $FFMPEG_COMMIT"
git clone --quiet --filter=blob:none --no-checkout https://github.com/FFmpeg/FFmpeg.git "$work/FFmpeg"
git -C "$work/FFmpeg" checkout --quiet "$FFMPEG_COMMIT"
full=$(git -C "$work/FFmpeg" rev-parse HEAD)
git -C "$work/FFmpeg" archive --format=tar --prefix=FFmpeg/ HEAD | xz -T0 > "$work/$name/FFmpeg-$full.tar.xz"

# The parts: the largest libraries first, each into the first part with room
# (the first part also holds FFmpeg, the build's scripts and the Dockerfile).
limit=1900000000
sizes=("$(du -cb "$work/$name"/*.tar.xz "$work/$name/Dockerfile" | tail -n 1 | cut -f 1)")
declare -A part_of
while read -r bytes file; do
  i=0
  while [ "$i" -lt "${#sizes[@]}" ] && [ $((sizes[i] + bytes)) -ge "$limit" ]; do
    i=$((i + 1))
  done
  [ "$i" -lt "${#sizes[@]}" ] || sizes+=(0)
  [ $((sizes[i] + bytes)) -lt "$limit" ] || die "$file alone is over $limit bytes"
  sizes[i]=$((sizes[i] + bytes))
  part_of[$file]=$i
done < <(cd "$work/$name/libraries" && find . -maxdepth 1 -type f -printf '%s %f\n' | sort -rn)
parts=${#sizes[@]}
for file in "${!part_of[@]}"; do
  i=${part_of[$file]}
  if [ "$i" -gt 0 ]; then
    mkdir -p "$work/part-$((i + 1))/$name/libraries"
    mv "$work/$name/libraries/$file" "$work/part-$((i + 1))/$name/libraries/"
  fi
done
spread=""
if [ "$parts" -gt 1 ]; then
  spread="
    They come in $parts files, since GitHub takes release files under 2 GiB:
    this one, then $name-2.tar and on. Unpack them in one place."
  for ((k = 2; k <= parts; k++)); do
    cat > "$work/part-$k/$name/PART-$k.txt" << EOF
Part $k of $parts of the source of the FFmpeg in flux's Windows installer: more
of libraries/. README.txt, in $name.tar, says what is what.
EOF
  done
fi

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
Dockerfile
    That build ($BTBN_BUILD), as FFmpeg-Builds' generate.sh writes it: the
    stages it runs, in order, and FFmpeg's configure flags at the end.
libraries/
    The source of each of the ${#used[@]} libraries the build compiles in, at
    its pinned commit, named as the build caches it: put them in
    FFmpeg-Builds/.cache/downloads/ and the build uses them as they are.$spread

To build it again: ./build.sh $BTBN_BUILD (FFmpeg-Builds/README.md).
flux runs ffmpeg.exe and ffprobe.exe as separate programs and links none of
FFmpeg's libraries.
EOF

tar -C "$work" -cf "$out/$name.tar" "$name"
for ((k = 2; k <= parts; k++)); do
  tar -C "$work/part-$k" -cf "$out/$name-$k.tar" "$name"
done
# GitHub refuses a release asset of 2 GiB or more.
for f in "$out/$name.tar" "$out/$name"-*.tar; do
  [ -f "$f" ] || continue
  size=$(stat -c %s "$f")
  echo "$(basename "$f"): $size bytes"
  [ "$size" -lt 2000000000 ] || die "$(basename "$f") is $size bytes: over GitHub's limit for a release asset"
done

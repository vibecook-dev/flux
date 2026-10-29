#!/usr/bin/env bash
# Collects the complete source of the FFmpeg the macOS app ships
# (ffmpeg-macos.env) into one tar, jellyfin-ffmpeg-<version>-macos-source.tar,
# which every release attaches: whoever passes a GPL program on offers its
# source, and Jellyfin publishes these builds as binaries. It holds:
#
#   jellyfin-ffmpeg-<commit>.tar.xz  jellyfin-ffmpeg at the commit the builds
#                                    are of: FFmpeg, Jellyfin's patches
#                                    (debian/patches) and the scripts that
#                                    built it (builder/, buildmac.sh)
#   libraries/<script>.tar.xz        the source of each library those scripts
#                                    build for macOS (builder/scripts.d), at
#                                    its pinned commit or revision
#   libraries/macos-<name>.*         the libraries builder/images/macos adds
#   README.txt                       what is where
#
# It runs where git, svn and curl are (GitHub's Ubuntu runners, with
# subversion installed).
#
# Usage: scripts/package/ffmpeg-macos-source.sh --out DIR
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
out=""
die() { echo "ffmpeg-macos-source.sh: $*" >&2; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --out) out=${2:?}; shift 2 ;;
    -h | --help) sed -n '2,22p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done
[ -n "$out" ] || die "usage: ffmpeg-macos-source.sh --out DIR"
mkdir -p "$out"
out=$(cd "$out" && pwd)

# The pins, read rather than sourced (macos-app.sh reads them too).
pin() {
  local v
  v=$(sed -n "s/^$1=//p" "$here/ffmpeg-macos.env")
  [ -n "$v" ] || die "$1 is not set in ffmpeg-macos.env"
  printf '%s' "$v"
}
VERSION=$(pin JELLYFIN_FFMPEG_VERSION)
COMMIT=$(pin JELLYFIN_FFMPEG_COMMIT)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
name="jellyfin-ffmpeg-$VERSION-macos-source"
mkdir -p "$work/$name/libraries"
jf="$work/jellyfin-ffmpeg"

# A git repository's tree at `ref` (a commit or a tag), with its submodules,
# as <dest>.tar.xz with one top folder, <top>.
git_tree() {
  local repo=$1 ref=$2 top=$3 dest=$4
  local dir="$work/clones/$top" what=FETCH_HEAD
  rm -rf "$dir"
  mkdir -p "$dir"
  git -C "$dir" init --quiet
  git -C "$dir" remote add origin "$repo"
  if ! git -C "$dir" fetch --quiet --depth 1 origin "$ref" 2> /dev/null; then
    # A server that won't hand out a commit by itself: the whole history.
    git -C "$dir" fetch --quiet --tags origin '+refs/heads/*:refs/remotes/origin/*'
    what=$ref
  fi
  git -C "$dir" -c advice.detachedHead=false checkout --quiet "$what"
  git -C "$dir" submodule update --quiet --init --recursive --depth 1 2> /dev/null ||
    git -C "$dir" submodule update --quiet --init --recursive
  tar -C "$work/clones" --exclude=.git -cf - "$top" | xz -T0 > "$dest"
  rm -rf "$dir"
}

echo "jellyfin-ffmpeg $VERSION at $COMMIT"
git clone --quiet --filter=blob:none --no-checkout https://github.com/jellyfin/jellyfin-ffmpeg.git "$jf"
git -C "$jf" -c advice.detachedHead=false checkout --quiet "$COMMIT"
grep -q "^jellyfin-ffmpeg ($VERSION)" "$jf/debian/changelog" || die "jellyfin-ffmpeg at $COMMIT isn't $VERSION (debian/changelog)"
git -C "$jf" archive --format=tar --prefix=jellyfin-ffmpeg/ HEAD | xz -T0 > "$work/$name/jellyfin-ffmpeg-$COMMIT.tar.xz"

# The libraries buildmac.sh builds: builder/scripts.d/*.sh (not its
# subfolders, which are Linux's), each where its ffbuild_enabled() says so
# for either Mac target, as buildmac.sh runs it (VARIANT gpl, no add-ins).
listed="$work/listed.txt"
: > "$listed"
for script in "$jf"/builder/scripts.d/*.sh; do
  base=$(basename "$script" .sh)
  # Its pins: SCRIPT_REPO/SCRIPT_COMMIT (or SCRIPT_REV for svn), then
  # SCRIPT_REPO2/SCRIPT_COMMIT2 and on.
  pins=$(
    for target in macarm64 mac64; do
      (
        # Jellyfin's scripts read these, and expect unset ones to be empty.
        set +u
        export TARGET=$target VARIANT=gpl ADDINS_STR=""
        # shellcheck disable=SC1090
        source "$script"
        [ -z "${SCRIPT_SKIP:-}" ] || exit 0
        ffbuild_enabled || exit 0
        for n in "" 2 3 4 5; do
          repo_var="SCRIPT_REPO$n" commit_var="SCRIPT_COMMIT$n" rev_var="SCRIPT_REV$n"
          [ -n "${!repo_var:-}" ] || continue
          if [ -n "${!rev_var:-}" ]; then
            echo "svn ${!repo_var} ${!rev_var} $n"
          else
            [ -n "${!commit_var:-}" ] || { echo "error $base: $repo_var without $commit_var" >&2; exit 1; }
            echo "git ${!repo_var} ${!commit_var} $n"
          fi
        done
      )
    done | sort -u
  )
  while read -r kind repo ref n; do
    [ -n "$kind" ] || continue
    file="$base${n:+-$n}.tar.xz"
    echo "  $file: $repo $ref"
    case "$kind" in
      git) git_tree "$repo" "$ref" "$base${n:+-$n}" "$work/$name/libraries/$file" ;;
      svn)
        svn export --quiet --non-interactive "$repo@$ref" "$work/clones/$base${n:+-$n}"
        tar -C "$work/clones" -cf - "$base${n:+-$n}" | xz -T0 > "$work/$name/libraries/$file"
        rm -rf "$work/clones/${base:?}${n:+-$n}"
        ;;
    esac
    echo "$file  $repo  $ref" >> "$listed"
  done <<< "$pins"
done

# builder/images/macos: the static libraries buildmac.sh adds first, each
# fetched with wget (a release tarball) or git clone and checkout.
for script in "$jf"/builder/images/macos/*.sh; do
  base=$(basename "$script" .sh)
  urls=$(grep -oE 'wget +https?://[^ ]+' "$script" | awk '{ print $2 }' || true)
  clone=$(grep -oE 'git clone +https?://[^ ]+' "$script" | awk '{ print $3 }' || true)
  ref=$(grep -oE 'git checkout +[^ ]+' "$script" | awk '{ print $3 }' || true)
  for url in $urls; do
    file="macos-$base-$(basename "$url")"
    echo "  $file: $url"
    curl -fsSL --retry 3 --proto '=https' -o "$work/$name/libraries/$file" "$url"
    echo "$file  $url" >> "$listed"
  done
  if [ -n "$clone" ]; then
    [ -n "$ref" ] || die "$base clones $clone without a checkout"
    file="macos-$base.tar.xz"
    echo "  $file: $clone $ref"
    git_tree "$clone" "$ref" "macos-$base" "$work/$name/libraries/$file"
    echo "$file  $clone  $ref" >> "$listed"
  fi
  if [ -z "$urls$clone" ] && grep -qE 'wget|curl|git clone|svn' "$script"; then
    die "$base fetches something this script doesn't follow: read builder/images/macos/$base.sh"
  fi
done
count=$(wc -l < "$listed")
[ "$count" -gt 10 ] || die "only $count libraries found: has builder/ changed?"
echo "$count libraries; the largest, in MB:"
(cd "$work/$name/libraries" && du -m -- * | sort -rn | head -10)

cat > "$work/$name/README.txt" << TXT
The source of the FFmpeg in flux's macOS app (flux.app/Contents/Helpers/ffmpeg):
jellyfin-ffmpeg $VERSION, the portable macOS builds Jellyfin publishes
($(pin ARM64_ASSET) and $(pin X86_64_ASSET)),
licensed under the GNU GPL version 3, made into one universal ffmpeg and one
ffprobe with lipo.

jellyfin-ffmpeg-$COMMIT.tar.xz
    https://github.com/jellyfin/jellyfin-ffmpeg at $COMMIT ($VERSION):
    FFmpeg, Jellyfin's patches to it (debian/patches), and the scripts that
    built these binaries (builder/buildmac.sh, builder/scripts.d/,
    builder/images/macos/, builder/variants/mac*-gpl.sh).
libraries/
    The source of each library those scripts compile in for macOS, at the
    commit or revision each script pins, named after the script:
$(sed 's/^/      /' "$listed")

To build it again: unpack jellyfin-ffmpeg, then run builder/buildmac.sh arm64
(or x86_64) on a Mac (builder/Buildmac.md); it fetches the same commits.
flux runs ffmpeg and ffprobe as separate programs and links none of FFmpeg's
libraries.
TXT

tar -C "$work" -cf "$out/$name.tar" "$name"
size=$(stat -c %s "$out/$name.tar" 2> /dev/null || stat -f %z "$out/$name.tar")
echo "$name.tar: $size bytes"
# GitHub refuses a release asset of 2 GiB or more.
[ "$size" -lt 2000000000 ] || die "$name.tar is $size bytes: over GitHub's limit for a release asset"

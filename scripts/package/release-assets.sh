#!/usr/bin/env bash
# Completes a release's asset directory, as release.yml uploads it and as the
# release dry run keeps it: checks that every target's package is there (a
# .tar.gz, or a .zip for Windows, and a .deb and .rpm for each Linux
# target), adds the source tarball of the jellyfin-ffmpeg the Docker image
# ships (GPL), and writes SHA256SUMS over all of it. Run it from the
# repository root.
#
# Usage: scripts/package/release-assets.sh --version <version> <dist-dir>
set -euo pipefail

targets="aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-pc-windows-msvc"
version="" dist=""
die() { echo "release-assets.sh: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) version=${2:?}; shift 2 ;;
    -h | --help) sed -n '2,8p' "$0"; exit 0 ;;
    -*) die "unknown argument: $1" ;;
    *) dist=$1; shift ;;
  esac
done
if [ -z "$version" ] || [ ! -d "$dist" ]; then die "usage: release-assets.sh --version <version> <dist-dir>"; fi

for t in $targets; do
  case "$t" in *-windows-*) package="flux-$version-$t.zip" ;; *) package="flux-$version-$t.tar.gz" ;; esac
  [ -s "$dist/$package" ] || die "missing $package in $dist"
done
for p in "flux-$version-amd64.deb" "flux-$version-arm64.deb" "flux-$version-x86_64.rpm" "flux-$version-aarch64.rpm"; do
  [ -s "$dist/$p" ] || die "missing $p in $dist"
done

jf=$(sed -n 's/^ARG JELLYFIN_FFMPEG_VERSION=//p' Dockerfile)
[ -n "$jf" ] || die "no JELLYFIN_FFMPEG_VERSION in Dockerfile"
curl -fsSL --retry 3 --proto '=https' -o "$dist/jellyfin-ffmpeg-$jf-source.tar.gz" \
  "https://github.com/jellyfin/jellyfin-ffmpeg/archive/refs/tags/v$jf.tar.gz"

(
  cd "$dist"
  rm -f SHA256SUMS
  if command -v sha256sum > /dev/null; then
    sha256sum -- *.tar.gz *.zip *.deb *.rpm > SHA256SUMS
  else
    shasum -a 256 -- *.tar.gz *.zip *.deb *.rpm > SHA256SUMS
  fi
  cat SHA256SUMS
)

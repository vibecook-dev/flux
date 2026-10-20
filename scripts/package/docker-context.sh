#!/usr/bin/env bash
# Lays out Linux release packages as the Dockerfile's `dist` build context:
#
#   <out>/amd64/   flux-<version>-x86_64-unknown-linux-gnu.tar.gz, extracted
#   <out>/arm64/   flux-<version>-aarch64-unknown-linux-gnu.tar.gz, extracted
#
# Usage: scripts/package/docker-context.sh [--version V] <packages-dir> <out>
# Then:  docker buildx build --build-context dist=<out> .
# A missing architecture is skipped with a warning (build only the other).
set -euo pipefail

version=""
die() { echo "docker-context.sh: $*" >&2; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --version) version=${2:?}; shift 2 ;;
    -h | --help) sed -n '2,10p' "$0"; exit 0 ;;
    -*) die "unknown argument: $1" ;;
    *) break ;;
  esac
done
[ $# -eq 2 ] || die "usage: docker-context.sh [--version V] <packages-dir> <out>"
pkgs=$1 out=$2
[ -d "$pkgs" ] || die "no such directory: $pkgs"

rm -rf "$out"
found=0
for pair in amd64:x86_64-unknown-linux-gnu arm64:aarch64-unknown-linux-gnu; do
  arch=${pair%%:*} target=${pair#*:}
  pkg="" count=0
  if [ -n "$version" ]; then
    candidates="$pkgs/flux-$version-$target.tar.gz"
  else
    candidates=$(find "$pkgs" -maxdepth 1 -name "flux-*-$target.tar.gz" | LC_ALL=C sort)
  fi
  while IFS= read -r f; do
    [ -f "$f" ] || continue
    pkg=$f
    count=$((count + 1))
  done <<< "$candidates"
  case $count in
    0) echo "docker-context.sh: warning: no $target package in $pkgs; skipping $arch" >&2; continue ;;
    1) ;;
    *) die "several $target packages in $pkgs; pass --version" ;;
  esac
  mkdir -p "$out/$arch"
  tar -xzf "$pkg" --strip-components=1 -C "$out/$arch"
  [ -x "$out/$arch/bin/flux-server" ] || die "$pkg has no bin/flux-server"
  echo "$out/$arch <- $(basename "$pkg")"
  found=$((found + 1))
done
[ "$found" -gt 0 ] || die "no Linux packages in $pkgs"

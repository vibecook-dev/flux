#!/usr/bin/env bash
# Packages a built flux-server as flux-<version>-<target>.tar.gz:
#
#   flux-<version>-<target>/
#     bin/flux-server
#     bin/sidecar-slim            (with --sidecar: Truffle's Tailscale sidecar)
#     share/flux/web/             (the web client; flux-server finds it at
#                                  <exe dir>/../share/flux/web)
#     LICENSE  README.md  THIRD_PARTY_LICENSES
#
# Usage:
#   scripts/package/package.sh --target aarch64-apple-darwin --version 0.1.0 \
#     --bin target/aarch64-apple-darwin/release/flux-server \
#     --web web/dist --third-party THIRD_PARTY_LICENSES [--sidecar FILE] \
#     [--license FILE] [--readme FILE] [--out DIR]
#
# SOURCE_DATE_EPOCH, when set, fixes the file times in the archive.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)

target="" version="" bin="" web="" third_party="" sidecar=""
license="$root/LICENSE" readme="$root/README.md" out="$root/dist"

die() { echo "package.sh: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --target) target=${2:?}; shift 2 ;;
    --version) version=${2:?}; shift 2 ;;
    --bin) bin=${2:?}; shift 2 ;;
    --web) web=${2:?}; shift 2 ;;
    --third-party) third_party=${2:?}; shift 2 ;;
    --sidecar) sidecar=${2:?}; shift 2 ;;
    --license) license=${2:?}; shift 2 ;;
    --readme) readme=${2:?}; shift 2 ;;
    --out) out=${2:?}; shift 2 ;;
    -h | --help) sed -n '2,18p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[ -n "$target" ] || die "--target is required"
[ -n "$version" ] || die "--version is required"
case "$version" in v*) die "--version takes the bare version (0.1.0), not the tag" ;; esac
if [ ! -f "$bin" ] || [ ! -x "$bin" ]; then die "--bin: not an executable file: ${bin:-<unset>}"; fi
[ -f "$web/index.html" ] || die "--web: no index.html in ${web:-<unset>} (build it with VITE_FLUX_API=/ pnpm build)"
[ -f "$third_party" ] || die "--third-party: no such file: ${third_party:-<unset>}"
[ -f "$license" ] || die "no LICENSE at $license"
[ -f "$readme" ] || die "no README at $readme"
if [ -n "$sidecar" ]; then [ -f "$sidecar" ] || die "--sidecar: no such file: $sidecar"; fi

name="flux-$version-$target"
work=$(mktemp -d "${TMPDIR:-/tmp}/flux-package.XXXXXX")
trap 'rm -rf "$work"' EXIT
stage="$work/$name"

mkdir -p "$stage/bin" "$stage/share/flux"
cp "$bin" "$stage/bin/flux-server"
chmod 0755 "$stage/bin/flux-server"
if [ -n "$sidecar" ]; then
  cp "$sidecar" "$stage/bin/sidecar-slim"
  chmod 0755 "$stage/bin/sidecar-slim"
fi
cp -R "$web" "$stage/share/flux/web"
cp "$license" "$stage/LICENSE"
cp "$readme" "$stage/README.md"
cp "$third_party" "$stage/THIRD_PARTY_LICENSES"
if [ -n "$sidecar" ]; then
  {
    printf '\n\n'
    cat "$here/sidecar-NOTICE.txt"
  } >> "$stage/THIRD_PARTY_LICENSES"
fi
find "$stage" -type d -exec chmod 0755 {} +
find "$stage" -type f ! -perm -u+x -exec chmod 0644 {} +

if [ -n "${SOURCE_DATE_EPOCH:-}" ]; then
  if stamp=$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y%m%d%H%M.%S 2> /dev/null); then :; else
    stamp=$(date -u -r "$SOURCE_DATE_EPOCH" +%Y%m%d%H%M.%S)
  fi
  find "$stage" -exec touch -h -t "$stamp" {} +
fi

mkdir -p "$out"
out=$(cd "$out" && pwd)
archive="$out/$name.tar.gz"
# Sorted entries, root-owned, no gzip timestamp: the same inputs give the
# same archive with GNU tar (CI) and close to it with bsdtar (macOS).
(
  cd "$work"
  if case "$(tar --version 2> /dev/null)" in *"GNU tar"*) true ;; *) false ;; esac; then
    tar --sort=name --owner=0 --group=0 --numeric-owner --format=gnu -cf - "$name"
  else
    find "$name" -print | LC_ALL=C sort > "$work/list"
    # No AppleDouble files, extended attributes, ACLs or file flags.
    COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs --no-acls --no-fflags \
      --uid 0 --gid 0 --uname root --gname root -n -cf - -T "$work/list"
  fi
) | gzip -9n > "$archive"

echo "$archive"

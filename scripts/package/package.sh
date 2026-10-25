#!/usr/bin/env bash
# Packages a built flux-server as flux-<version>-<target>.tar.gz:
#
#   flux-<version>-<target>/
#     bin/flux-server
#     bin/flux                    (with --cli: the command line, for people and agents)
#     bin/sidecar-slim            (with --sidecar: Truffle's Tailscale sidecar)
#     share/flux/web/             (the web client; flux-server finds it at
#                                  <exe dir>/../share/flux/web)
#     LICENSE  README.md  THIRD_PARTY_LICENSES
#
# or, for a Windows target, as flux-<version>-<target>.zip, with everything
# in one folder and a short README of its own (README-windows.txt):
#
#   flux-<version>-<target>/
#     flux-server.exe
#     flux.exe                    (with --cli)
#     sidecar-slim.exe            (with --sidecar)
#     web/                        (flux-server finds it at <exe dir>/web)
#     LICENSE.txt  README.txt  THIRD_PARTY_LICENSES.txt
#
# Usage:
#   scripts/package/package.sh --target aarch64-apple-darwin --version 0.1.0 \
#     --bin target/aarch64-apple-darwin/release/flux-server \
#     --web web/dist --third-party THIRD_PARTY_LICENSES [--cli FILE] [--sidecar FILE] \
#     [--license FILE] [--readme FILE] [--out DIR]
#
# SOURCE_DATE_EPOCH, when set, fixes the file times in the archive. A zip
# needs Python 3 (zip.py).
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)

target="" version="" bin="" web="" third_party="" sidecar="" cli=""
license="$root/LICENSE" readme="" out="$root/dist"

die() { echo "package.sh: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --target) target=${2:?}; shift 2 ;;
    --version) version=${2:?}; shift 2 ;;
    --bin) bin=${2:?}; shift 2 ;;
    --web) web=${2:?}; shift 2 ;;
    --third-party) third_party=${2:?}; shift 2 ;;
    --sidecar) sidecar=${2:?}; shift 2 ;;
    --cli) cli=${2:?}; shift 2 ;;
    --license) license=${2:?}; shift 2 ;;
    --readme) readme=${2:?}; shift 2 ;;
    --out) out=${2:?}; shift 2 ;;
    -h | --help) sed -n '2,31p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[ -n "$target" ] || die "--target is required"
[ -n "$version" ] || die "--version is required"
# Where each file goes: a Unix tree, or one Windows folder.
case "$target" in
  *-windows-*)
    windows=true exe=.exe txt=.txt bindir="" webdir=web readme_as=README.txt
    [ -n "$readme" ] || readme="$here/README-windows.txt"
    ;;
  *)
    windows=false exe="" txt="" bindir=bin/ webdir=share/flux/web readme_as=README.md
    [ -n "$readme" ] || readme="$root/README.md"
    ;;
esac
case "$version" in v*) die "--version takes the bare version (0.1.0), not the tag" ;; esac
if [ ! -f "$bin" ] || [ ! -x "$bin" ]; then die "--bin: not an executable file: ${bin:-<unset>}"; fi
[ -f "$web/index.html" ] || die "--web: no index.html in ${web:-<unset>} (build it with VITE_FLUX_API=/ pnpm build)"
[ -f "$third_party" ] || die "--third-party: no such file: ${third_party:-<unset>}"
[ -f "$license" ] || die "no LICENSE at $license"
[ -f "$readme" ] || die "no README at $readme"
if [ -n "$sidecar" ]; then [ -f "$sidecar" ] || die "--sidecar: no such file: $sidecar"; fi
if [ -n "$cli" ]; then { [ -f "$cli" ] && [ -x "$cli" ]; } || die "--cli: not an executable file: $cli"; fi

# sidecar-NOTICE.txt has the licences of the Go modules in the pinned
# sidecars. Every module this sidecar was built from (its build info, as
# `go version -m` prints it) must be in the notice's list of modules.
if [ -n "$sidecar" ]; then
  modules=$(LC_ALL=C tr -c '[:print:]\t\n' '\n' < "$sidecar" |
    grep -E $'^(dep|=>)\t[^\t]+\t[^\t]+' | cut -f2,3 | tr '\t' ' ' | LC_ALL=C sort -u) || true
  [ -n "$modules" ] || die "--sidecar: no Go module list in $sidecar"
  listed=$(awk '/^Go modules:$/ { f = 1; next } f && /^  / { print $1, $2; next } f { exit }' \
    "$here/sidecar-NOTICE.txt" | LC_ALL=C sort -u)
  missing=$(LC_ALL=C comm -23 <(printf '%s\n' "$modules") <(printf '%s\n' "$listed"))
  [ -z "$missing" ] || die "scripts/package/sidecar-NOTICE.txt lacks these Go modules of $sidecar (regenerate it with scripts/package/sidecar-notice.py):
$missing"
fi

# A Python 3 for zip.py. On Windows, `python3` can be the Microsoft Store's
# stand-in, which only prints how to install Python.
python=""
if [ "$windows" = true ]; then
  for p in python3 python py; do
    if "$p" -c 'import sys; sys.exit(sys.version_info < (3, 8))' > /dev/null 2>&1; then
      python=$p
      break
    fi
  done
  [ -n "$python" ] || die "a zip package needs Python 3.8 or later (python3, python or py)"
fi

name="flux-$version-$target"
work=$(mktemp -d "${TMPDIR:-/tmp}/flux-package.XXXXXX")
trap 'rm -rf "$work"' EXIT
stage="$work/$name"

mkdir -p "$stage/${bindir}" "$(dirname "$stage/$webdir")"
cp "$bin" "$stage/${bindir}flux-server$exe"
chmod 0755 "$stage/${bindir}flux-server$exe"
if [ -n "$cli" ]; then
  cp "$cli" "$stage/${bindir}flux$exe"
  chmod 0755 "$stage/${bindir}flux$exe"
fi
if [ -n "$sidecar" ]; then
  cp "$sidecar" "$stage/${bindir}sidecar-slim$exe"
  chmod 0755 "$stage/${bindir}sidecar-slim$exe"
fi
cp -R "$web" "$stage/$webdir"
cp "$license" "$stage/LICENSE$txt"
cp "$readme" "$stage/$readme_as"
cp "$third_party" "$stage/THIRD_PARTY_LICENSES$txt"
if [ -n "$sidecar" ]; then
  {
    printf '\n\n'
    cat "$here/sidecar-NOTICE.txt"
  } >> "$stage/THIRD_PARTY_LICENSES$txt"
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
if [ "$windows" = true ]; then
  archive="$out/$name.zip"
  rm -f "$archive"
  "$python" "$here/zip.py" create "$stage" "$archive" --epoch "${SOURCE_DATE_EPOCH:-$(date +%s)}"
  echo "$archive"
  exit 0
fi
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

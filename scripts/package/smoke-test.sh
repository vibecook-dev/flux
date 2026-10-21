#!/usr/bin/env bash
# Unpacks a flux release package and checks it runs: `flux-server --version`,
# then a server on 127.0.0.1:<port> with an empty library and a scratch data
# dir, `GET /` (the web client) and `GET /api/v1/server`, then SIGTERM.
# The server is started through a symlink, as docs/INSTALL.md installs it,
# so it must find its web client next to the real binary. The tailnet stays
# off.
#
# On macOS it also checks the minimum macOS that bin/flux-server and
# bin/sidecar-slim declare (`otool -arch all -l`, LC_BUILD_VERSION minos)
# against the MACOS_* versions below, and runs an x86_64-apple-darwin package
# on Apple Silicon under Rosetta 2 (`arch -x86_64`).
#
# Usage: scripts/package/smoke-test.sh dist/flux-<version>-<target>.tar.gz
#          [--port 18097] [--dir DIR] [--keep]
set -euo pipefail

# The macOS versions docs/INSTALL.md (Requirements) promises, also in
# docs/RELEASING.md's target table. A binary that needs a newer macOS fails
# the test; to raise one, change it here and in both docs together.
MACOS_FLUX_SERVER=11.0
MACOS_SIDECAR=13.0

archive="" port=18097 dir="" keep=false
die() { echo "smoke-test.sh: $*" >&2; exit 1; }

# version_gt A B: whether the dotted version A is above B.
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

# check_minos FILE MAX: no LC_BUILD_VERSION minos in FILE (one per
# architecture in a universal binary: plain `otool -l` shows only the host's)
# may be above MAX.
check_minos() {
  local file=$1 max=$2 load minos seen=""
  load=$(otool -arch all -l "$file") || die "otool -arch all -l $file failed (it comes with Xcode's command line tools)"
  while IFS= read -r minos; do
    if version_gt "$minos" "$max"; then
      die "$(basename "$file") needs macOS $minos, but docs/INSTALL.md promises macOS $max"
    fi
    seen="$seen $minos"
  done < <(awk '$1 == "cmd" { cmd = $2 } cmd == "LC_BUILD_VERSION" && $1 == "minos" { print $2 }' <<< "$load")
  [ -n "$seen" ] || die "no LC_BUILD_VERSION minos in $file"
  echo "== $(basename "$file"): minos$seen (INSTALL.md promises $max)"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --port) port=${2:?}; shift 2 ;;
    --dir) dir=${2:?}; shift 2 ;;
    --keep) keep=true; shift ;;
    -h | --help) sed -n '2,15p' "$0"; exit 0 ;;
    -*) die "unknown argument: $1" ;;
    *) archive=$1; shift ;;
  esac
done
[ -f "$archive" ] || die "usage: smoke-test.sh <package.tar.gz> [--port N] [--dir DIR] [--keep]"
[ "$port" != 8096 ] || die "port 8096 is flux's default; pick another for a test"

name=$(basename "$archive" .tar.gz)
version="" target=""
for t in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
  case "$name" in flux-*-"$t") target=$t version=${name#flux-}; version=${version%-"$t"} ;; esac
done
[ -n "$version" ] || die "not a flux package name: $name (flux-<version>-<target>.tar.gz)"

# How to run flux-server: directly, or under Rosetta 2 for an Intel package
# on Apple Silicon.
run=()
if [ "$target" = x86_64-apple-darwin ] && [ "$(uname -s)/$(uname -m)" = Darwin/arm64 ]; then
  arch -x86_64 /usr/bin/true 2> /dev/null ||
    die "the x86_64 package needs Rosetta 2 on this Mac: softwareupdate --install-rosetta --agree-to-license"
  run=(arch -x86_64)
fi

if [ -z "$dir" ]; then
  dir=$(mktemp -d "${TMPDIR:-/tmp}/flux-smoke.XXXXXX")
else
  rm -rf "$dir"
  mkdir -p "$dir"
fi
dir=$(cd "$dir" && pwd) # absolute, for the symlink below
pid=""
cleanup() {
  if [ -n "$pid" ] && kill -0 "$pid" 2> /dev/null; then kill -9 "$pid" 2> /dev/null || true; fi
  if [ "$keep" = false ]; then rm -rf "$dir"; fi
}
trap cleanup EXIT

tar -xzf "$archive" -C "$dir"
root="$dir/$name"
bin="$root/bin/flux-server"
[ -x "$bin" ] || die "no executable bin/flux-server in $archive"
[ -f "$root/share/flux/web/index.html" ] || die "no share/flux/web/index.html in $archive"
for f in LICENSE README.md THIRD_PARTY_LICENSES; do
  [ -s "$root/$f" ] || die "no $f in $archive"
done

case "$target" in
  *-apple-darwin)
    check_minos "$bin" "$MACOS_FLUX_SERVER"
    if [ -e "$root/bin/sidecar-slim" ]; then
      check_minos "$root/bin/sidecar-slim" "$MACOS_SIDECAR"
    else
      echo "== no bin/sidecar-slim in this package"
    fi
    ;;
esac

if [ ${#run[@]} -gt 0 ]; then
  echo "== running x86_64 flux-server under Rosetta 2: ${run[*]}"
fi
said=$(${run[@]+"${run[@]}"} "$bin" --version)
echo "== $said"
if [ "$version" != dev ]; then
  case "$said" in *"$version"*) ;; *) die "flux-server --version does not say $version" ;; esac
fi
# The ffmpeg capability check, where this flux-server has one.
help=$(${run[@]+"${run[@]}"} "$bin" --help 2>&1 || true)
if case "$help" in *--check*) true ;; *) false ;; esac; then
  echo "== flux-server --check"
  ${run[@]+"${run[@]}"} "$bin" --check || echo "(--check reported a problem; not fatal here)"
fi

mkdir -p "$dir/data" "$dir/library" "$dir/link"
log="$dir/server.log"
# Like `ln -sf /opt/flux/bin/flux-server /usr/local/bin/flux-server`.
ln -s "$bin" "$dir/link/flux-server"
# Run from an empty directory so ./web/dist can't stand in for the bundled
# client: the server must find share/flux/web next to its binary. No FLUX_*
# variable from the caller's environment (FLUX_LAN, FLUX_WEB_DIR, …) applies.
unset_flux=()
while IFS= read -r v; do unset_flux+=(-u "$v"); done < <(compgen -e | grep '^FLUX_' || true)
(
  cd "$dir"
  exec env ${unset_flux[@]+"${unset_flux[@]}"} FLUX_TAILNET=off ${run[@]+"${run[@]}"} "$dir/link/flux-server" \
    --data-dir "$dir/data" --bind 127.0.0.1 --port "$port" \
    --library "Smoke:movies:$dir/library" --tailnet off
) > "$log" 2>&1 &
pid=$!

base="http://127.0.0.1:$port"
for _ in $(seq 1 60); do
  if curl -fsS -o /dev/null "$base/api/v1/server" 2> /dev/null; then break; fi
  kill -0 "$pid" 2> /dev/null || { cat "$log" >&2; die "flux-server exited early"; }
  sleep 0.5
done

info=$(curl -fsS "$base/api/v1/server") || { cat "$log" >&2; die "GET /api/v1/server failed"; }
echo "== GET /api/v1/server: $info"
case "$info" in *'"version"'*) ;; *) die "unexpected /api/v1/server body" ;; esac
if [ "$version" != dev ]; then
  case "$info" in *"\"version\":\"$version\""*) ;; *) die "/api/v1/server does not report version $version" ;; esac
fi

page=$(curl -fsS "$base/") || { cat "$log" >&2; die "GET / failed"; }
# Only the built client has the root element; the page flux serves when it
# can't find the client is HTML too.
case "$page" in *'<div id="root">'*) ;; *) cat "$log" >&2; die "GET / is not the web client" ;; esac
echo "== GET /: $(printf '%s' "$page" | wc -c | tr -d ' ') bytes of HTML"

kill -TERM "$pid"
for _ in $(seq 1 40); do
  kill -0 "$pid" 2> /dev/null || break
  sleep 0.25
done
if kill -0 "$pid" 2> /dev/null; then
  cat "$log" >&2
  die "flux-server did not exit within 10 s of SIGTERM"
fi
wait "$pid" || true
pid=""
echo "== ok: $name"

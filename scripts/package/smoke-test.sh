#!/usr/bin/env bash
# Unpacks a flux release package and checks it runs: `flux-server --version`,
# then a server on 127.0.0.1:<port> with an empty library and a scratch data
# dir, `GET /` (the web client) and `GET /api/v1/server`, then SIGTERM.
# The tailnet stays off.
#
# Usage: scripts/package/smoke-test.sh dist/flux-<version>-<target>.tar.gz
#          [--port 18097] [--dir DIR] [--keep]
set -euo pipefail

archive="" port=18097 dir="" keep=false
die() { echo "smoke-test.sh: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --port) port=${2:?}; shift 2 ;;
    --dir) dir=${2:?}; shift 2 ;;
    --keep) keep=true; shift ;;
    -h | --help) sed -n '2,8p' "$0"; exit 0 ;;
    -*) die "unknown argument: $1" ;;
    *) archive=$1; shift ;;
  esac
done
[ -f "$archive" ] || die "usage: smoke-test.sh <package.tar.gz> [--port N] [--dir DIR] [--keep]"
[ "$port" != 8096 ] || die "port 8096 is flux's default; pick another for a test"

name=$(basename "$archive" .tar.gz)
version=""
for t in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
  case "$name" in flux-*-"$t") version=${name#flux-}; version=${version%-"$t"} ;; esac
done
[ -n "$version" ] || die "not a flux package name: $name (flux-<version>-<target>.tar.gz)"

if [ -z "$dir" ]; then
  dir=$(mktemp -d "${TMPDIR:-/tmp}/flux-smoke.XXXXXX")
else
  rm -rf "$dir"
  mkdir -p "$dir"
fi
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

said=$("$bin" --version)
echo "== $said"
if [ "$version" != dev ]; then
  case "$said" in *"$version"*) ;; *) die "flux-server --version does not say $version" ;; esac
fi
# The ffmpeg capability check, where this flux-server has one.
help=$("$bin" --help 2>&1 || true)
if case "$help" in *--check*) true ;; *) false ;; esac; then
  echo "== flux-server --check"
  "$bin" --check || echo "(--check reported a problem; not fatal here)"
fi

mkdir -p "$dir/data" "$dir/library"
log="$dir/server.log"
# Run from an empty directory so ./web/dist can't stand in for the bundled
# client: the server must find share/flux/web next to its binary. No FLUX_*
# variable from the caller's environment (FLUX_LAN, FLUX_WEB_DIR, …) applies.
unset_flux=()
while IFS= read -r v; do unset_flux+=(-u "$v"); done < <(compgen -e | grep '^FLUX_' || true)
(
  cd "$dir"
  exec env ${unset_flux[@]+"${unset_flux[@]}"} FLUX_TAILNET=off "$bin" \
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
case "$page" in *'<div id="root">'* | *'<!doctype html>'* | *'<!DOCTYPE html>'*) ;; *) die "GET / is not the web client" ;; esac
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

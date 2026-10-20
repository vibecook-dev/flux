#!/usr/bin/env bash
# Runs a flux image with an empty library and checks it: the health check
# turns healthy, GET / serves the web client and GET /api/v1/server answers,
# through the published port. Requests from the host reach flux from the
# Docker network, so they are anonymous LAN callers ("path":"lan", "you":
# null); a request from inside the container is the Owner.
#
# Usage: scripts/package/docker-smoke-test.sh <image> [version] [--port 18096]
set -euo pipefail

image="" version="" port=18096
die() { echo "docker-smoke-test.sh: $*" >&2; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --port) port=${2:?}; shift 2 ;;
    -h | --help) sed -n '2,8p' "$0"; exit 0 ;;
    -*) die "unknown argument: $1" ;;
    *) if [ -z "$image" ]; then image=$1; else version=$1; fi; shift ;;
  esac
done
[ -n "$image" ] || die "usage: docker-smoke-test.sh <image> [version] [--port N]"
[ "$port" != 8096 ] || die "use a port other than 8096 for a test"

name="flux-smoke-$$"
cleanup() {
  docker rm -f -v "$name" > /dev/null 2>&1 || true
  docker volume rm -f "$name-config" > /dev/null 2>&1 || true
}
trap cleanup EXIT
fail() {
  echo "--- container logs" >&2
  docker logs "$name" >&2 || true
  die "$*"
}

docker run -d --name "$name" \
  -p "127.0.0.1:$port:8096" \
  -v "$name-config:/config" \
  -e FLUX_LIBRARIES="Smoke:movies:/media" \
  "$image" > /dev/null

# The first health check runs 30 s after start (the Dockerfile's interval).
echo "== waiting for the health check"
status=""
for _ in $(seq 1 90); do
  status=$(docker inspect -f '{{.State.Health.Status}}' "$name" 2> /dev/null || echo gone)
  case "$status" in
    healthy) break ;;
    unhealthy | gone) fail "container is $status" ;;
  esac
  sleep 1
done
[ "$status" = healthy ] || fail "not healthy after 90 s (status: $status)"

base="http://127.0.0.1:$port"
info=$(curl -fsS "$base/api/v1/server") || fail "GET /api/v1/server failed"
echo "== GET /api/v1/server from the host: $info"
case "$info" in *'"path":"lan"'*) ;; *) fail "expected a LAN (anonymous) caller from the host" ;; esac
if [ -n "$version" ] && [ "$version" != dev ]; then
  case "$info" in *"\"version\":\"$version\""*) ;; *) fail "image does not report version $version" ;; esac
fi
page=$(curl -fsS "$base/") || fail "GET / failed"
case "$page" in *'<div id="root">'* | *'<!doctype html>'* | *'<!DOCTYPE html>'*) ;; *) fail "GET / is not the web client" ;; esac
echo "== GET /: $(printf '%s' "$page" | wc -c | tr -d ' ') bytes of HTML"

inside=$(docker exec "$name" curl -fsS http://127.0.0.1:8096/api/v1/server) || fail "GET from inside failed"
echo "== GET /api/v1/server inside the container: $inside"
case "$inside" in *'"path":"localhost"'*) ;; *) fail "expected a localhost caller inside the container" ;; esac

echo "== $(docker exec "$name" flux-server --version)"
echo "== $(docker exec "$name" sh -c '"$FLUX_FFMPEG" -hide_banner -version | sed -n 1p')"
docker exec "$name" sh -c '"$FLUX_FFPROBE" -hide_banner -version > /dev/null' || fail "ffprobe does not run"
docker exec "$name" test -s /usr/share/licenses/jellyfin-ffmpeg/copyright || fail "no ffmpeg licence"
docker exec "$name" test -s /usr/share/licenses/jellyfin-ffmpeg/SOURCE-OFFER.md || fail "no SOURCE-OFFER"
docker exec "$name" test -s /opt/flux/THIRD_PARTY_LICENSES || fail "no THIRD_PARTY_LICENSES"
[ "$(docker exec "$name" id -u)" = 1000 ] || fail "not running as uid 1000"

echo "== stopping (SIGTERM)"
start=$(date +%s)
docker stop -t 15 "$name" > /dev/null
code=$(docker inspect -f '{{.State.ExitCode}}' "$name")
echo "== exited $code after $(($(date +%s) - start)) s"
[ "$code" = 0 ] || [ "$code" = 143 ] || fail "exit code $code"
echo "== ok: $image"

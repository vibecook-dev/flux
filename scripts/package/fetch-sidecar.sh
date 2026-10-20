#!/usr/bin/env bash
# Downloads Truffle's Tailscale sidecar (`sidecar-slim`) for one release
# target, so a packaged flux-server can run `--tailnet embedded` without a Go
# toolchain. flux-server looks for `sidecar-slim` next to its own binary.
#
# The sidecar must come from the same Truffle release as the truffle-core
# crate flux was built with, so the version is read from Cargo.lock (or given
# with --version / TRUFFLE_SIDECAR_VERSION). The download must match a
# SHA-256 pinned in truffle-sidecar.sha256; a mismatch is an error.
#
# Nothing is bundled (exit 0, with a warning) when truffle-core is not a
# crates.io release, when no checksum is pinned for that version, or when the
# Truffle release has no asset for the target.
#
# Usage:
#   scripts/package/fetch-sidecar.sh --target <triple> --out <dir> [--version V]
#   scripts/package/fetch-sidecar.sh --pin <version>   # record checksums
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
sums="$here/truffle-sidecar.sha256"
repo=${TRUFFLE_REPOSITORY:-vibecook-dev/truffle}
targets="aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu"

target="" out="" version=${TRUFFLE_SIDECAR_VERSION:-} pin=""

die() { echo "fetch-sidecar.sh: $*" >&2; exit 1; }
skip() {
  if [ -n "${GITHUB_ACTIONS:-}" ]; then
    echo "::warning title=Truffle sidecar not bundled::$*"
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
      printf '> [!WARNING]\n> **Truffle sidecar not bundled%s:** %s\n\n' "${target:+ for $target}" "$*" >> "$GITHUB_STEP_SUMMARY"
    fi
  else
    echo "fetch-sidecar.sh: not bundling the sidecar: $*" >&2
  fi
  exit 0
}

asset_for() {
  case "$1" in
    aarch64-apple-darwin) echo tsnet-sidecar-darwin-arm64 ;;
    x86_64-apple-darwin) echo tsnet-sidecar-darwin-amd64 ;;
    x86_64-unknown-linux-gnu) echo tsnet-sidecar-linux-amd64 ;;
    aarch64-unknown-linux-gnu) echo tsnet-sidecar-linux-arm64 ;;
    *) return 1 ;;
  esac
}

sha256_of() {
  if command -v sha256sum > /dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# Prints the HTTP status; the body goes to $2.
download() {
  curl -sSL --retry 3 --proto '=https' -o "$2" -w '%{http_code}' \
    "https://github.com/$repo/releases/download/truffle-v$1/$3"
}

# "<version> <source>" of truffle-core in Cargo.lock; source is empty for a
# path dependency.
lock_truffle() {
  awk '
    function flush() { if (name == "truffle-core") print version, source; name = ""; version = ""; source = "" }
    /^\[\[package\]\]/ { flush(); next }
    /^name = / { gsub(/"/, "", $3); name = $3 }
    /^version = / { gsub(/"/, "", $3); version = $3 }
    /^source = / { gsub(/"/, "", $3); source = $3 }
    END { flush() }
  ' "$root/Cargo.lock"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --target) target=${2:?}; shift 2 ;;
    --out) out=${2:?}; shift 2 ;;
    --version) version=${2:?}; shift 2 ;;
    --pin) pin=${2:?}; shift 2 ;;
    -h | --help) sed -n '2,19p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

work=$(mktemp -d "${TMPDIR:-/tmp}/flux-sidecar.XXXXXX")
trap 'rm -rf "$work"' EXIT

if [ -n "$pin" ]; then
  pin=${pin#v}
  pin=${pin#truffle-v}
  lines=""
  for t in $targets; do
    a=$(asset_for "$t")
    code=$(download "$pin" "$work/$a" "$a")
    [ "$code" = 200 ] || die "truffle-v$pin/$a: HTTP $code"
    lines+="$(sha256_of "$work/$a")  $pin/$a"$'\n'
  done
  grep -v "  $pin/" "$sums" > "$work/sums" || true
  printf '%s' "$lines" >> "$work/sums"
  cp "$work/sums" "$sums"
  printf 'Pinned Truffle %s sidecars in %s:\n%s' "$pin" "$sums" "$lines"
  exit 0
fi

[ -n "$target" ] || die "--target is required"
[ -n "$out" ] || die "--out is required"
asset=$(asset_for "$target") || die "no Truffle sidecar for target $target"

if [ -z "$version" ]; then
  read -r version source < <(lock_truffle) || true
  [ -n "$version" ] || skip "truffle-core is not in Cargo.lock"
  case "${source:-}" in
    registry+*) ;;
    "") skip "truffle-core $version comes from a local path, not a Truffle release" ;;
    *) skip "truffle-core $version comes from $source, not a Truffle release" ;;
  esac
fi

want=$(awk -v key="$version/$asset" '$2 == key { print $1 }' "$sums")
[ -n "$want" ] || skip "no checksum pinned for Truffle $version ($asset); run: scripts/package/fetch-sidecar.sh --pin $version"

code=$(download "$version" "$work/$asset" "$asset")
case "$code" in
  200) ;;
  404) skip "Truffle release truffle-v$version has no asset $asset" ;;
  *) die "downloading truffle-v$version/$asset: HTTP $code" ;;
esac
got=$(sha256_of "$work/$asset")
[ "$got" = "$want" ] || die "truffle-v$version/$asset: SHA-256 $got, expected $want"

mkdir -p "$out"
cp "$work/$asset" "$out/sidecar-slim"
chmod 0755 "$out/sidecar-slim"
echo "$out/sidecar-slim (Truffle $version, $asset, sha256 $got)"

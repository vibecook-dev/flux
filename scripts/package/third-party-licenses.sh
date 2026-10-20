#!/usr/bin/env bash
# Writes THIRD_PARTY_LICENSES for the release packages: the licences of the
# Rust crates in flux-server (cargo-about with about.toml/about.hbs) and of
# the npm packages in the web client (web-licenses.mjs). package.sh appends
# the sidecar's notices when it bundles one.
#
# Usage: scripts/package/third-party-licenses.sh [--web FILE] [--out FILE]
#   --web FILE   output of web-licenses.mjs; default: run it on web/
#   --out FILE   default: THIRD_PARTY_LICENSES
#
# Needs cargo-about 0.9 (`cargo install cargo-about --locked`, or
# taiki-e/install-action in CI) and, without --web, pnpm and web/node_modules.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
web="" out="THIRD_PARTY_LICENSES"

die() { echo "third-party-licenses.sh: $*" >&2; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --web) web=${2:?}; shift 2 ;;
    --out) out=${2:?}; shift 2 ;;
    -h | --help) sed -n '2,13p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done
cargo about --version > /dev/null 2>&1 || die "cargo-about is not installed"

work=$(mktemp -d "${TMPDIR:-/tmp}/flux-licenses.XXXXXX")
trap 'rm -rf "$work"' EXIT

# Only flux-server's dependency graph: that is what the packages ship.
cargo about generate --locked --fail \
  --config "$here/about.toml" \
  --manifest-path "$root/crates/flux-server/Cargo.toml" \
  "$here/about.hbs" > "$work/rust.txt"

if [ -z "$web" ]; then
  node "$here/web-licenses.mjs" "$root/web" > "$work/web.txt"
  web="$work/web.txt"
fi
[ -s "$web" ] || die "empty web licence list: $web"

{
  cat << 'EOF'
THIRD-PARTY SOFTWARE NOTICES AND LICENCES

flux is MIT-licensed (see LICENSE). It includes the third-party software
listed below, each under its own licence.

This software is based in part on the work of the Independent JPEG Group.

flux does not contain FFmpeg. It runs the ffmpeg and ffprobe programs that
are installed on the system (or bundled in the Docker image, with their own
licence in /usr/share/licenses/jellyfin-ffmpeg) as separate processes.

EOF
  cat "$work/rust.txt"
  cat << 'EOF'

================================================================================
Part 2: fonts embedded in bin/flux-server (poster typography)
================================================================================

EOF
  # Each font ships its licence as OFL-<name>.txt next to it.
  for f in "$root"/crates/*/assets/fonts/OFL-*.txt; do
    [ -f "$f" ] || continue
    name=$(basename "$f" .txt)
    rule=--------------------------------------------------------------------------------
    printf '%s\n%s (SIL Open Font License 1.1), %s\n%s\n\n' "$rule" "${name#OFL-}" "${f#"$root"/}" "$rule"
    cat "$f"
    printf '\n'
  done
  cat << 'EOF'

================================================================================
Part 3: npm packages bundled into the web client (share/flux/web)
================================================================================

EOF
  cat "$web"
} > "$out"

echo "$out: $(wc -l < "$out" | tr -d ' ') lines"

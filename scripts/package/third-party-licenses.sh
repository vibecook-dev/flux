#!/usr/bin/env bash
# Writes THIRD_PARTY_LICENSES for the release packages: the licences of the
# Rust crates in flux-server (cargo-about with about.toml/about.hbs), the
# notices in crate sources that cargo-about leaves out (the table below), and
# the licences of the npm packages in the web client (web-licenses.mjs).
# package.sh appends the sidecar's notices when it bundles one.
#
# Usage: scripts/package/third-party-licenses.sh [--web FILE] [--out FILE]
#   --web FILE   output of web-licenses.mjs; default: run it on web/
#   --out FILE   default: THIRD_PARTY_LICENSES
#
# Needs cargo-about 0.9 (`cargo install cargo-about --locked`, or
# taiki-e/install-action in CI), jq and, without --web, pnpm and
# web/node_modules.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
web="" out="THIRD_PARTY_LICENSES"

die() { echo "third-party-licenses.sh: $*" >&2; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --web) web=${2:?}; shift 2 ;;
    --out) out=${2:?}; shift 2 ;;
    -h | --help) sed -n '2,14p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done
cargo about --version > /dev/null 2>&1 || die "cargo-about is not installed"
command -v jq > /dev/null || die "jq is not installed"

# Notices that cargo-about leaves out of Part 1. It quotes the licence each
# crate declares, which covers the crate's own code, and takes the text from
# the crate's licence files when it recognises them. So it misses:
# - C code that a crate compiles in under that code's own licence;
# - a licence file it doesn't recognise: it then quotes the licence's
#   generic text, without the crate's copyright lines.
# One line per crate: its name, the file in its source that holds the notice
# ("-": no file is needed), and what the notice covers or why none is needed.
# Every crate in flux-server's graph that compiles C or assembly (a build
# dependency on cc or cmake) must have a line.
notices='
tikv-jemalloc-sys jemalloc/COPYING jemalloc, the C memory allocator it compiles in (BSD-2-Clause)
libsqlite3-sys - the SQLite it compiles in is in the public domain
ring - its C and assembly code is under the licences quoted for ring in Part 1
blake3 - its C and assembly code is under the licences quoted for blake3 in Part 1
libm LICENSE.txt its copyright lines, with those of musl libc and the other C code it is ported from (MIT)
rcgen LICENSE its copyright line (MIT OR Apache-2.0)
'

work=$(mktemp -d "${TMPDIR:-/tmp}/flux-licenses.XXXXXX")
trap 'rm -rf "$work"' EXIT

# Only flux-server's dependency graph: that is what the packages ship.
cargo about generate --locked --fail \
  --config "$here/about.toml" \
  --manifest-path "$root/crates/flux-server/Cargo.toml" \
  "$here/about.hbs" > "$work/rust.txt"

# flux-server's graph on about.toml's targets, one crate per line:
# "<name> <version> <compiles C: true|false> <source dir>". These are its
# normal dependencies, as cargo-about counts them. cargo metadata resolves
# features for the whole workspace, so this can list a few crates that
# Part 1 doesn't.
platforms=()
while read -r t; do
  platforms+=(--filter-platform "$t")
done < <(sed -n '/^targets = \[/,/^\]/s/^ *"\([^"]*\)".*/\1/p' "$here/about.toml")
[ ${#platforms[@]} -gt 0 ] || die "no targets in $here/about.toml"
cargo metadata --locked --format-version 1 \
  --manifest-path "$root/crates/flux-server/Cargo.toml" "${platforms[@]}" |
  jq -r '
    def deps($kind): map({key: .id, value: [.deps[] | select(any(.dep_kinds[]; .kind == $kind))]}) | from_entries;
    def closure($next): . as $ids
      | ($ids + [$ids[] | $next[.][].pkg] | unique) as $more
      | if ($more | length) == ($ids | length) then $ids else ($more | closure($next)) end;
    (.resolve.nodes | deps(null)) as $normal
    | (.resolve.nodes | deps("build")) as $build
    | ([.resolve.root] | closure($normal)) as $ids
    | .packages[]
    | select(.id as $id | $ids | index($id))
    | (any($build[.id][]; .name == "cc" or .name == "cmake")) as $c
    | "\(.name) \(.version) \($c) \(.manifest_path | rtrimstr("/Cargo.toml"))"
  ' > "$work/graph.txt"
grep -q '^flux-server ' "$work/graph.txt" || die "cargo metadata found no flux-server"

while read -r name version c _; do
  if [ "$c" = true ] && ! grep -q "^$name " <<< "$notices"; then
    die "$name $version compiles C or assembly into flux-server: check the licence of that code, and add $name to the table in $0"
  fi
done < "$work/graph.txt"

# Part 2: first the crates that need no file, then each file, read from the
# source of the locked version that cargo builds. A crate in the graph whose
# file is missing stops the script.
rule=--------------------------------------------------------------------------------
{
  while read -r crate file what; do
    if [ -z "$crate" ] || [ "$file" != - ]; then continue; fi
    while read -r name version _ _; do
      if [ "$name" = "$crate" ]; then printf '%s %s: %s.\n' "$name" "$version" "$what"; fi
    done < "$work/graph.txt"
  done <<< "$notices"
  while read -r crate file what; do
    if [ -z "$crate" ] || [ "$file" = - ]; then continue; fi
    while read -r name version _ dir; do
      if [ "$name" != "$crate" ]; then continue; fi
      [ -s "$dir/$file" ] || die "$name $version is in flux-server's graph, but its notice, $dir/$file, is missing"
      printf '\n%s\n%s %s, %s:\n%s\n%s\n\n' "$rule" "$name" "$version" "$file" "$what" "$rule"
      cat "$dir/$file"
    done < "$work/graph.txt"
  done <<< "$notices"
} > "$work/notices.txt"

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
Part 2: notices in crate sources that Part 1 leaves out
================================================================================

Part 1 quotes the licence that each crate declares. The notices below, from
the crates' sources, cover C code that a crate compiles in under that code's
own licence, and copyright lines that Part 1 leaves out where it quotes a
licence's generic text.

EOF
  cat "$work/notices.txt"
  cat << 'EOF'

================================================================================
Part 3: fonts embedded in bin/flux-server (poster typography)
================================================================================

EOF
  # Each font ships its licence as OFL-<name>.txt next to it.
  for f in "$root"/crates/*/assets/fonts/OFL-*.txt; do
    [ -f "$f" ] || continue
    name=$(basename "$f" .txt)
    printf '%s\n%s (SIL Open Font License 1.1), %s\n%s\n\n' "$rule" "${name#OFL-}" "${f#"$root"/}" "$rule"
    cat "$f"
    printf '\n'
  done
  cat << 'EOF'

================================================================================
Part 4: npm packages in the web client (share/flux/web)
================================================================================

EOF
  cat "$web"
} > "$out"

echo "$out: $(wc -l < "$out" | tr -d ' ') lines"

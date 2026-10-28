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
# A Windows package (.zip) is tested on Windows, in Git Bash, and needs
# Python 3 (zip.py, pe-imports.py) and Windows PowerShell. First,
# flux-server.exe and sidecar-slim.exe must import no DLL of the Visual C++
# Redistributable, which a fresh Windows lacks and the test machine likely
# has (pe-imports.py reads the import tables). flux-server.exe then runs
# from the unpacked folder in a hidden console of its own
# (windows-console.ps1) and is stopped as a service wrapper (NSSM, WinSW)
# stops it: once with Ctrl-C, then, started again on the same data dir, with
# Ctrl-Break. Each stop must log "flux stopped" and exit 0.
#
# Usage: scripts/package/smoke-test.sh dist/flux-<version>-<target>.tar.gz|.zip
#          [--port 18097] [--dir DIR] [--keep]
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

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
    -h | --help) sed -n '2,25p' "$0"; exit 0 ;;
    -*) die "unknown argument: $1" ;;
    *) archive=$1; shift ;;
  esac
done
[ -f "$archive" ] || die "usage: smoke-test.sh <package.tar.gz|.zip> [--port N] [--dir DIR] [--keep]"
[ "$port" != 8096 ] || die "port 8096 is flux's default; pick another for a test"

case "$archive" in
  *.zip) name=$(basename "$archive" .zip) ext=zip ;;
  *) name=$(basename "$archive" .tar.gz) ext=tar.gz ;;
esac
version="" target=""
for t in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-pc-windows-msvc; do
  case "$name" in flux-*-"$t") target=$t version=${name#flux-}; version=${version%-"$t"} ;; esac
done
[ -n "$version" ] || die "not a flux package name: $name (flux-<version>-<target>.tar.gz, or .zip for Windows)"

windows=false
case "$target" in *-windows-*) windows=true ;; esac
if [ "$windows" = true ]; then
  [ "$ext" = zip ] || die "a Windows package is a .zip: $archive"
  case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) ;;
    *) die "a Windows package is tested on Windows, in Git Bash" ;;
  esac
  # On Windows, `python3` can be the Microsoft Store's stand-in.
  python=""
  for p in python3 python py; do
    if "$p" -c 'import sys; sys.exit(sys.version_info < (3, 8))' > /dev/null 2>&1; then
      python=$p
      break
    fi
  done
  [ -n "$python" ] || die "testing a zip package needs Python 3.8 or later (python3, python or py)"
  console=(powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass
    -File "$(cygpath -w "$here/windows-console.ps1")")
else
  [ "$ext" = tar.gz ] || die "only a Windows package is a .zip: $archive"
fi

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
# Whether the server started below still runs. $pid is a Windows process id
# on Windows, which Git Bash's kill doesn't know.
alive() {
  if [ "$windows" = true ]; then
    tasklist //NH //FI "PID eq $pid" 2> /dev/null | grep -q flux-server
  else
    kill -0 "$pid" 2> /dev/null
  fi
}
cleanup() {
  if [ -n "$pid" ] && alive; then
    if [ "$windows" = true ]; then
      "${console[@]}" kill "$pid" || true
    else
      kill -9 "$pid" 2> /dev/null || true
    fi
  fi
  if [ "$keep" = false ]; then rm -rf "$dir"; fi
}
trap cleanup EXIT

root="$dir/$name"
if [ "$windows" = true ]; then
  "$python" "$here/zip.py" extract "$archive" "$dir"
  bin="$root/flux-server.exe"
  [ -f "$bin" ] || die "no flux-server.exe in $archive"
  [ -f "$root/web/index.html" ] || die "no web/index.html in $archive"
  marker="$root/install-kind"
  docs="LICENSE.txt README.txt THIRD_PARTY_LICENSES.txt"
  if [ -f "$root/sidecar-slim.exe" ]; then
    echo "== sidecar-slim.exe: $(wc -c < "$root/sidecar-slim.exe" | tr -d ' ') bytes"
  else
    echo "== no sidecar-slim.exe in this package"
  fi
else
  tar -xzf "$archive" -C "$dir"
  bin="$root/bin/flux-server"
  [ -x "$bin" ] || die "no executable bin/flux-server in $archive"
  [ -f "$root/share/flux/web/index.html" ] || die "no share/flux/web/index.html in $archive"
  marker="$root/share/flux/install-kind"
  docs="LICENSE README.md THIRD_PARTY_LICENSES"
fi
# The updater (flux-server update, "Update now") updates a package that says so.
[ "$(cat "$marker" 2> /dev/null)" = archive ] || die "no install-kind saying \"archive\" in $archive"
for f in $docs; do
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
  *-windows-*)
    # Running the exe here proves nothing about a fresh Windows: this
    # machine (a GitHub runner, a developer's) likely has the Visual C++
    # Redistributable, so vcruntime140.dll would load. Read the imports.
    exes=(flux-server.exe)
    if [ -f "$root/flux.exe" ]; then exes+=(flux.exe); fi
    if [ -f "$root/sidecar-slim.exe" ]; then exes+=(sidecar-slim.exe); fi
    if ! (cd "$root" && "$python" "$here/pe-imports.py" --no-vc-redist "${exes[@]}") | sed 's/^/== imports of /'; then
      die "the package's .exe files must start without the Visual C++ Redistributable (see above)"
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
# The command line, when the package has it.
if [ "$windows" = true ]; then cli="$root/flux.exe"; else cli="$root/bin/flux"; fi
if [ -f "$cli" ]; then
  cli_said=$(${run[@]+"${run[@]}"} "$cli" --version)
  echo "== $cli_said"
  if [ "$version" != dev ]; then
    case "$cli_said" in *"$version"*) ;; *) die "flux --version does not say $version" ;; esac
  fi
else
  echo "== no flux command line in this package"
fi
# The ffmpeg capability check, where this flux-server has one.
help=$(${run[@]+"${run[@]}"} "$bin" --help 2>&1 || true)
if case "$help" in *--check*) true ;; *) false ;; esac; then
  echo "== flux-server --check"
  ${run[@]+"${run[@]}"} "$bin" --check || echo "(--check reported a problem; not fatal here)"
fi

mkdir -p "$dir/data" "$dir/library"
# No FLUX_* variable from the caller's environment (FLUX_LAN, FLUX_WEB_DIR,
# …) applies.
unset_flux=()
while IFS= read -r v; do unset_flux+=(-u "$v"); done < <(compgen -e | grep '^FLUX_' || true)
base="http://127.0.0.1:$port"

# start LOG: starts the server with its output in LOG and sets $pid. It runs
# from an empty directory so ./web/dist can't stand in for the bundled
# client: the server must find its web client next to its binary.
start() {
  local log=$1
  if [ "$windows" = true ]; then
    pid=$(cd "$dir" && env ${unset_flux[@]+"${unset_flux[@]}"} FLUX_TAILNET=off "${console[@]}" start \
      "$(cygpath -w "$log")" "$(cygpath -w "$bin")" --data-dir "$(cygpath -w "$dir/data")" \
      --bind 127.0.0.1 --port "$port" --library "Smoke:movies:$(cygpath -w "$dir/library")" \
      --tailnet off | tr -d '\r') || die "could not start $bin"
    case "$pid" in '' | *[!0-9]*) die "windows-console.ps1 start printed '$pid', not a process id" ;; esac
  else
    # Like `ln -sf /opt/flux/bin/flux-server /usr/local/bin/flux-server`.
    mkdir -p "$dir/link"
    ln -sf "$bin" "$dir/link/flux-server"
    (
      cd "$dir"
      exec env ${unset_flux[@]+"${unset_flux[@]}"} FLUX_TAILNET=off ${run[@]+"${run[@]}"} "$dir/link/flux-server" \
        --data-dir "$dir/data" --bind 127.0.0.1 --port "$port" \
        --library "Smoke:movies:$dir/library" --tailnet off
    ) > "$log" 2>&1 &
    pid=$!
  fi
  for _ in $(seq 1 60); do
    if curl -fsS -o /dev/null "$base/api/v1/server" 2> /dev/null; then return 0; fi
    alive || { cat "$log" >&2; die "flux-server exited early"; }
    sleep 0.5
  done
  cat "$log" >&2
  die "flux-server did not answer on $base within 30 s"
}

# stop LOG HOW: stops the server and checks that it stopped gracefully.
# HOW is TERM (SIGTERM) or, on Windows, c (Ctrl-C) or break (Ctrl-Break).
stop() {
  local log=$1 how=$2 code key
  if [ "$windows" = true ]; then
    case "$how" in c) key=Ctrl-C ;; *) key=Ctrl-Break ;; esac
    code=$("${console[@]}" stop "$pid" "$how" 10 | tr -d '\r') || {
      cat "$log" >&2
      die "flux-server did not exit within 10 s of $key: $code"
    }
    [ "$code" = 0 ] || { cat "$log" >&2; die "flux-server exited with $code after $key"; }
    grep -q 'flux stopped' "$log" || { cat "$log" >&2; die "no \"flux stopped\" in the log after $key"; }
    echo "== $key: \"flux stopped\", exit code 0"
  else
    kill "-$how" "$pid"
    for _ in $(seq 1 40); do
      alive || break
      sleep 0.25
    done
    if alive; then
      cat "$log" >&2
      die "flux-server did not exit within 10 s of SIG$how"
    fi
    wait "$pid" || true
  fi
  pid=""
}

start "$dir/server.log"
info=$(curl -fsS "$base/api/v1/server") || { cat "$dir/server.log" >&2; die "GET /api/v1/server failed"; }
echo "== GET /api/v1/server: $info"
case "$info" in *'"version"'*) ;; *) die "unexpected /api/v1/server body" ;; esac
if [ "$version" != dev ]; then
  case "$info" in *"\"version\":\"$version\""*) ;; *) die "/api/v1/server does not report version $version" ;; esac
fi

page=$(curl -fsS "$base/") || { cat "$dir/server.log" >&2; die "GET / failed"; }
# Only the built client has the root element; the page flux serves when it
# can't find the client is HTML too.
case "$page" in *'<div id="root">'*) ;; *) cat "$dir/server.log" >&2; die "GET / is not the web client" ;; esac
echo "== GET /: $(printf '%s' "$page" | wc -c | tr -d ' ') bytes of HTML"

if [ "$windows" = true ]; then
  stop "$dir/server.log" c
  # Again on the same data dir, so the catalog reopens after a stop.
  start "$dir/server2.log"
  echo "== restarted on the same data dir"
  stop "$dir/server2.log" break
else
  stop "$dir/server.log" TERM
fi
echo "== ok: $name"

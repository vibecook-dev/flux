#!/usr/bin/env bash
# Installs the .deb and .rpm (linux-packages.sh) in throwaway containers of
# the systems they're for, and checks them: flux-server runs and knows how
# it was installed, the files and the `flux` account are there, the service's
# config file survives a removal, and a purge (deb) or erase leaves no
# program behind. Needs Docker (GitHub's Ubuntu runners have it).
#
# Usage: scripts/package/linux-smoke-test.sh --dist DIR --version VERSION
set -euo pipefail

dist="" version=""
die() { echo "linux-smoke-test.sh: $*" >&2; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --dist) dist=${2:?}; shift 2 ;;
    --version) version=${2:?}; shift 2 ;;
    -h | --help) sed -n '2,8p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done
if [ ! -d "$dist" ] || [ -z "$version" ]; then die "usage: linux-smoke-test.sh --dist DIR --version VERSION"; fi
case "$(uname -m)" in
  x86_64) deb_arch=amd64 rpm_arch=x86_64 ;;
  aarch64 | arm64) deb_arch=arm64 rpm_arch=aarch64 ;;
  *) die "no packages for $(uname -m)" ;;
esac
deb="flux-$version-$deb_arch.deb"
rpm="flux-$version-$rpm_arch.rpm"
[ -s "$dist/$deb" ] || die "missing $deb in $dist"
[ -s "$dist/$rpm" ] || die "missing $rpm in $dist"
dist=$(cd "$dist" && pwd)

# What every install must look like ($1: deb or rpm). It runs in the
# container, so its variables stay unexpanded here.
# shellcheck disable=SC2016
checks='
set -eu
kind=$1
v=$(flux-server --version)
echo "$v"
case "$v" in *"'"$version"'"*) ;; *) echo "wrong version: $v"; exit 1 ;; esac
test "$(cat /usr/share/flux/install-kind)" = "$kind"
test -f /usr/share/flux/web/index.html
test -f /usr/lib/systemd/system/flux.service
test -f /usr/lib/sysusers.d/flux.conf
test -f /etc/flux/flux.toml
test -x /usr/bin/flux
getent passwd flux
# The service config file, as the service reads it (no ffmpeg here: that is
# what --check says, so its status is not the point).
flux-server --config /etc/flux/flux.toml --check || true
'

echo "::group::Debian (trixie): $deb"
docker run --rm -v "$dist:/dist:ro" debian:trixie-slim sh -c "
  set -eu
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends /dist/$deb
  sh -c '$checks' check deb
  echo '# edited' >> /etc/flux/flux.toml
  apt-get remove -y -qq flux
  test ! -e /usr/bin/flux-server
  grep -q '# edited' /etc/flux/flux.toml
  apt-get purge -y -qq flux
  test ! -e /etc/flux/flux.toml
"
echo "::endgroup::"

for image in fedora:latest almalinux:10; do
  echo "::group::$image: $rpm"
  docker run --rm -v "$dist:/dist:ro" "$image" sh -c "
    set -eu
    dnf install -y -q --setopt=install_weak_deps=False /dist/$rpm
    sh -c '$checks' check rpm
    echo '# edited' >> /etc/flux/flux.toml
    dnf remove -y -q flux
    test ! -e /usr/bin/flux-server
    test -e /etc/flux/flux.toml.rpmsave
  "
  echo "::endgroup::"
done
echo "The .deb and .rpm install, run and remove cleanly."

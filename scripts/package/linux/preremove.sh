#!/bin/sh
# Before flux's files are removed (deb prerm, rpm %preun): stop the service
# when flux is going away, not when it's being upgraded.
set -e
case "$1" in
    remove)
        if [ -z "${DPKG_ROOT:-}" ] && [ -d /run/systemd/system ]; then
            deb-systemd-invoke stop flux.service >/dev/null || true
        fi
        ;;
    0)
        # rpm, erased: as %systemd_preun.
        if [ -d /run/systemd/system ]; then
            systemctl --no-reload disable --now flux.service >/dev/null 2>&1 || true
        fi
        ;;
esac

#!/bin/sh
# After flux's files are removed (deb postrm, rpm %postun). A purge (deb)
# also removes the data in /var/lib/flux; a plain removal keeps it.
set -e
case "$1" in
    remove)
        if [ -d /run/systemd/system ]; then
            systemctl --system daemon-reload >/dev/null || true
        fi
        ;;
    purge)
        if command -v deb-systemd-helper >/dev/null 2>&1; then
            deb-systemd-helper purge flux.service >/dev/null || true
            deb-systemd-helper unmask flux.service >/dev/null || true
        fi
        rm -rf /var/lib/flux
        ;;
    1)
        # rpm, upgraded: as %systemd_postun_with_restart.
        if [ -d /run/systemd/system ]; then
            systemctl try-restart flux.service >/dev/null 2>&1 || true
        fi
        ;;
esac

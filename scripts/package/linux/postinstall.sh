#!/bin/sh
# After flux's files are in place (deb postinst, rpm %post). Debian and
# Ubuntu enable and start the service, and restart it after an upgrade, as
# dh_installsystemd does. Fedora and its kin leave it off until it's enabled
# (`systemctl enable --now flux`): their policy for a service that can listen
# on the network, which `lan` in /etc/flux/flux.toml makes it.
set -e
# The GPU, for hardware transcoding (VAAPI, Quick Sync).
for group in render video; do
    if getent group "$group" >/dev/null 2>&1; then
        usermod -a -G "$group" flux >/dev/null 2>&1 || true
    fi
done
case "$1" in
    configure | abort-upgrade | abort-deconfigure | abort-remove)
        # deb: $2 is the version being upgraded from, if any.
        if command -v deb-systemd-helper >/dev/null 2>&1; then
            deb-systemd-helper unmask flux.service >/dev/null || true
            if deb-systemd-helper --quiet was-enabled flux.service; then
                deb-systemd-helper enable flux.service >/dev/null || true
            else
                deb-systemd-helper update-state flux.service >/dev/null || true
            fi
        fi
        if [ -z "${DPKG_ROOT:-}" ] && [ -d /run/systemd/system ]; then
            systemctl --system daemon-reload >/dev/null || true
            if [ -n "${2:-}" ]; then action=restart; else action=start; fi
            deb-systemd-invoke "$action" flux.service >/dev/null || true
        fi
        ;;
    1)
        # rpm, a first install: the system's presets decide (off, by default).
        if [ -d /run/systemd/system ]; then
            systemctl --no-reload preset flux.service >/dev/null 2>&1 || true
        fi
        echo "flux is installed. Start it now and at every boot: sudo systemctl enable --now flux"
        ;;
esac

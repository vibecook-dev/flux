#!/bin/sh
# Before flux's files are unpacked (deb preinst, rpm %pre): the account the
# service runs as, from the same line as /usr/lib/sysusers.d/flux.conf.
set -e
if command -v systemd-sysusers >/dev/null 2>&1; then
    printf 'u flux - "flux media server" /var/lib/flux\n' |
        systemd-sysusers --replace=/usr/lib/sysusers.d/flux.conf -
elif ! getent passwd flux >/dev/null 2>&1; then
    useradd --system --user-group --home-dir /var/lib/flux --no-create-home \
        --shell /usr/sbin/nologin --comment "flux media server" flux
fi

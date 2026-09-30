#!/bin/sh
# After the package is unpacked: the service account, the config file's ownership, and
# systemd. deb passes "configure [previous-version]"; rpm passes 1 on install and 2 on
# upgrade.
set -e

if command -v systemd-sysusers >/dev/null 2>&1; then
    systemd-sysusers discoclip.conf
else
    getent group discoclip >/dev/null || groupadd --system discoclip
    getent passwd discoclip >/dev/null || useradd --system --gid discoclip \
        --home-dir /var/lib/discoclip --shell /usr/sbin/nologin --comment "DiscoClip" discoclip
fi

chgrp discoclip /etc/discoclip/discoclip.toml
chmod 0640 /etc/discoclip/discoclip.toml

first_install=false
case "$1" in
    configure) [ -z "${2:-}" ] && first_install=true ;;
    1) first_install=true ;;
esac

if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    if [ "$first_install" = true ]; then
        systemctl enable --now discoclip.service
    elif systemctl is-active --quiet discoclip.service; then
        systemctl restart discoclip.service
    fi
fi

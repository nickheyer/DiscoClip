#!/bin/sh
# After the package is removed. deb passes "remove", "purge" or "upgrade"; rpm passes 0
# on removal and 1 on upgrade. A purge removes the config directory. The database,
# backups and published files under /var/lib/discoclip and the cache under
# /var/cache/discoclip stay, as does the service account.
set -e

if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
fi

case "$1" in
    purge)
        rm -rf /etc/discoclip
        ;;
esac

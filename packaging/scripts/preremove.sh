#!/bin/sh
# Before the package is removed. deb passes "remove", "upgrade" or "deconfigure"; rpm
# passes 0 on removal and 1 on upgrade. A removal stops the service; an upgrade leaves
# it running for postinstall to restart.
set -e

case "$1" in
    remove|0)
        if [ -d /run/systemd/system ]; then
            systemctl disable --now discoclip.service || true
        fi
        ;;
esac

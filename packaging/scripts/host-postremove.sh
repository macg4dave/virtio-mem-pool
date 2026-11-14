#!/bin/sh
# After removing the host package. /etc/rampire and /var/lib/rampire stay;
# purging (Debian) removes both. RPM has no purge: remove them by hand.
set -e
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
fi
if [ "$1" = purge ]; then
    rm -rf /etc/rampire /var/lib/rampire
fi

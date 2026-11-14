#!/bin/sh
# After removing the guest package. Its durable request state stays: the
# host may already have admitted the request it holds. Purging (Debian) is
# the explicit discard.
set -e
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
fi
if [ "$1" = purge ]; then
    rm -rf /var/lib/private/rampire-guest /var/lib/rampire-guest
fi

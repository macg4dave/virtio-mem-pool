#!/bin/sh
# Before removing the guest package (not before an upgrade).
set -e
if [ "$1" = remove ] || [ "$1" = 0 ]; then
    if [ -d /run/systemd/system ]; then
        systemctl disable --now rampire-guest.service || true
    fi
fi

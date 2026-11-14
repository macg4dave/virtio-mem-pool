#!/bin/sh
# After installing or upgrading the guest package. It is enabled on first
# install only, so an administrator's `disable` survives upgrades, and
# restarted onto the new binary every time. The request state carries over.
set -e
first=false
# Debian: "configure" with no previous version. RPM: 1 on first install.
if { [ "$1" = configure ] && [ -z "$2" ]; } || [ "$1" = 1 ]; then
    first=true
fi
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    if $first; then
        systemctl enable rampire-guest.service
    fi
    if systemctl is-enabled --quiet rampire-guest.service; then
        systemctl restart rampire-guest.service
    fi
fi

#!/bin/sh
# After installing or upgrading the host package. The controller isn't
# enabled here: it needs `rampire init` (or a configuration and
# `rampire apply`) first. An upgrade restarts a running controller onto
# the new binary.
set -e
if command -v systemd-sysusers >/dev/null 2>&1; then
    systemd-sysusers /usr/lib/sysusers.d/rampire.conf
fi
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    if systemctl is-active --quiet rampire.service; then
        systemctl restart rampire.service
    fi
fi

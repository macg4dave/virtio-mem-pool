#!/bin/sh
# Before removing the host package (not before an upgrade): stop the
# controller. Each VM keeps the memory it has.
set -e
# Debian passes "remove"; RPM passes 0 when the package is erased.
if [ "$1" = remove ] || [ "$1" = 0 ]; then
    if [ -d /run/systemd/system ]; then
        systemctl disable --now rampire.service || true
    fi
fi

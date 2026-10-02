#!/bin/sh
set -e
# Debian passes remove/deconfigure; RPM passes 0 on removal and 1 on upgrade.
case "${1:-}" in
    remove|deconfigure|0)
        if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
            systemctl disable --now redir-rust.service || true
        fi
        ;;
esac

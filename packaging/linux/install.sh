#!/bin/sh
# BackupUtility — Linux installer.
#
# `bkgui`, started from the unpacked folder, offers to install itself. This
# script is the same install for a terminal, or a machine without a desktop:
# it runs `bkengine install`, which copies the binaries to their home, links
# them into your PATH and adds an application-menu entry.
#
#   as a user   ~/.local/share/BackupUtility, ~/.local/bin, ~/.local/share/applications
#   as root     /opt/backuputility, /usr/local/bin, /usr/share/applications
#
# Jobs, settings and run state are never touched by an install, and an
# uninstall keeps them.
#
#   sh install.sh               install or upgrade
#   sh install.sh --desktop     also put a shortcut on the desktop
#   sh install.sh --uninstall   remove the program, its links and menu entry

set -eu

here=$(cd "$(dirname "$0")" && pwd)

for f in bkengine bkgui; do
    if [ ! -f "$here/$f" ]; then
        echo "error: $here/$f not found — run this script from the unpacked release folder" >&2
        exit 1
    fi
done
chmod +x "$here/bkengine" "$here/bkgui" 2>/dev/null || true

case "${1:-}" in
    "")          exec "$here/bkengine" install ;;
    --desktop)   exec "$here/bkengine" install --desktop-shortcut ;;
    --uninstall) exec "$here/bkengine" uninstall --yes ;;
    *)           echo "usage: sh install.sh [--desktop | --uninstall]" >&2; exit 2 ;;
esac

#!/bin/sh
# BackupUtility — Linux installer.
#
# Installs for the current user (no root needed, which is what SteamOS wants):
#   binaries      ~/.local/share/BackupUtility/{bkengine,bkgui}
#   on your PATH  ~/.local/bin/{bkengine,bkgui}  (symlinks)
#   app menu      ~/.local/share/applications/backuputility.desktop
#
# Run as root it installs system-wide instead:
#   /opt/backuputility, /usr/local/bin, /usr/share/applications
#
# The updater (bkgui's banner, or `bkengine update`) later swaps the binaries
# in place, so this script only runs once.
#
#   sh install.sh              install or upgrade
#   sh install.sh --uninstall  remove the files this script created
#                              (jobs, settings and run state are kept)

set -eu

here=$(cd "$(dirname "$0")" && pwd)

if [ "$(id -u)" -eq 0 ]; then
    app_dir=/opt/backuputility
    bin_dir=/usr/local/bin
    desktop_dir=/usr/share/applications
    icon_dir=/usr/share/icons/hicolor/256x256/apps
else
    data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
    app_dir="$data_home/BackupUtility"
    bin_dir="$HOME/.local/bin"
    desktop_dir="$data_home/applications"
    icon_dir="$data_home/icons/hicolor/256x256/apps"
fi

uninstall() {
    rm -f "$bin_dir/bkengine" "$bin_dir/bkgui"
    rm -f "$desktop_dir/backuputility.desktop" "$icon_dir/backuputility.png"
    rm -rf "$app_dir"
    echo "removed BackupUtility from $app_dir (jobs, settings and state were kept)"
}

if [ "${1:-}" = "--uninstall" ]; then
    uninstall
    exit 0
fi

for f in bkengine bkgui; do
    if [ ! -f "$here/$f" ]; then
        echo "error: $here/$f not found — run this script from the unpacked release folder" >&2
        exit 1
    fi
done

mkdir -p "$app_dir" "$bin_dir" "$desktop_dir" "$icon_dir"
install -m 755 "$here/bkengine" "$app_dir/bkengine"
install -m 755 "$here/bkgui" "$app_dir/bkgui"
[ -f "$here/icon.png" ] && install -m 644 "$here/icon.png" "$icon_dir/backuputility.png"
ln -sf "$app_dir/bkengine" "$bin_dir/bkengine"
ln -sf "$app_dir/bkgui" "$bin_dir/bkgui"

cat > "$desktop_dir/backuputility.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=BackupUtility
Comment=Mirror a path to another drive, on a schedule
Exec=$app_dir/bkgui
Icon=backuputility
Terminal=false
Categories=System;Utility;
StartupWMClass=bkgui
EOF
chmod 644 "$desktop_dir/backuputility.desktop"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$desktop_dir" >/dev/null 2>&1 || true

echo "installed BackupUtility $("$app_dir/bkengine" version | head -1 | awk '{print $2}') to $app_dir"
echo "  run the window:   bkgui"
echo "  command line:     bkengine --help"
case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) echo "  note: $bin_dir is not on your PATH yet; log out and in, or use the full path" ;;
esac

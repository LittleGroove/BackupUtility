#!/bin/bash
# End-to-end check of the installer on Linux, for whichever user runs it:
# a normal user exercises the ~/.local layout, root the /opt layout.
#
# Stages a copy of the release binaries in a temporary folder, registers a
# throwaway scheduled job from there, installs from the copy through
# `install.sh --desktop` (which runs `bkengine install --desktop-shortcut`),
# checks the program folder, the PATH links, the menu entry, the icon, the
# desktop shortcut and the re-pointed timer, then uninstalls with
# `bkengine uninstall --yes` and checks that everything is gone and the job
# file was kept.
#
# Refuses to run when this user already has an install or registered timers:
# the uninstall step would remove them.
#
#   cargo build --release --workspace
#   bash scripts/install_e2e.sh
#
# Honours CARGO_TARGET_DIR (the binaries come from <target>/release).
set -u

root=$(cd "$(dirname "$0")/.." && pwd)
rel="${CARGO_TARGET_DIR:-$root/target}/release"
work=${TMPDIR:-/tmp}/bku-install-e2e
src="$work/src"
job=bku-install-e2e
pass=0; fail=0
check() { local desc=$1; shift; if "$@" >/dev/null 2>&1; then echo "PASS  $desc"; pass=$((pass+1)); else echo "FAIL  $desc"; fail=$((fail+1)); fi; }

if [ "$(id -u)" -eq 0 ]; then
    home_dir=/opt/backuputility
    bin_dir=/usr/local/bin
    app_dir=/usr/share/applications
    icon_dir=/usr/share/icons/hicolor/256x256/apps
    unit_dir=/etc/systemd/system
    sysctl="systemctl"
else
    data=${XDG_DATA_HOME:-$HOME/.local/share}
    home_dir="$data/BackupUtility"
    bin_dir="$HOME/.local/bin"
    app_dir="$data/applications"
    icon_dir="$data/icons/hicolor/256x256/apps"
    unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    sysctl="systemctl --user"
fi
desk=$(xdg-user-dir DESKTOP 2>/dev/null || true)
if [ -z "$desk" ] || [ "$desk" = "$HOME" ]; then desk="$HOME/Desktop"; fi

echo "=== preconditions"
[ -x "$rel/bkgui" ] || { echo "no release build in $rel; run cargo build --release --workspace first" >&2; exit 1; }
[ ! -e "$home_dir" ] || { echo "$home_dir exists: a real install, which the round-trip would remove" >&2; exit 1; }
if ls "$unit_dir"/bku-*.timer >/dev/null 2>&1; then
    echo "registered bku-*.timer units exist in $unit_dir, and the uninstall step would remove them" >&2; exit 1
fi

echo "=== stage a source copy"
rm -rf "$work"
mkdir -p "$src" "$work/srcA" "$work/dstB"
cp "$rel/bkengine" "$rel/bkgui" "$root/LICENSE" "$root/packaging/linux/install.sh" "$src/"
echo hello > "$work/srcA/file.txt"
cd "$src"

echo "=== a scheduled job pointing at the source copy"
./bkengine jobs add --name "$job" --source "$work/srcA" --dest "$work/dstB" --every daily --at 03:00
./bkengine schedule install --job "$job"
check "timer registered with the source engine" grep -q "ExecStart=$src/bkengine" "$unit_dir/bku-$job.service"
check "source copy says it is not the installed one" sh -c "./bkengine version | grep -q 'not the installed one'"

echo "=== sh install.sh --desktop, from the source copy"
sh install.sh --desktop
check "bkengine installed" test -x "$home_dir/bkengine"
check "bkgui installed" test -x "$home_dir/bkgui"
check "LICENSE installed" test -f "$home_dir/LICENSE"
check "no scratch dir left" test ! -e "$home_dir/.bku-install"
check "PATH link bkengine" test "$(readlink "$bin_dir/bkengine")" = "$home_dir/bkengine"
check "PATH link bkgui" test "$(readlink "$bin_dir/bkgui")" = "$home_dir/bkgui"
check "menu entry points at the installed gui" grep -q "Exec=$home_dir/bkgui" "$app_dir/backuputility.desktop"
check "icon installed" test -s "$icon_dir/backuputility.png"
check "desktop shortcut ($desk)" test -x "$desk/backuputility.desktop"
check "timer re-pointed at the installed engine" grep -q "ExecStart=$home_dir/bkengine" "$unit_dir/bku-$job.service"
check "timer still active" $sysctl is-active "bku-$job.timer"
check "installed copy (via the PATH link) says it is the installed one" sh -c "'$bin_dir/bkengine' version | grep -q 'this is the installed copy'"

echo "=== install again from home (refresh only)"
"$home_dir/bkengine" install
check "still installed" test -x "$home_dir/bkgui"

echo "=== bkengine uninstall --yes, from the installed copy"
"$home_dir/bkengine" uninstall --yes
check "home removed" test ! -e "$home_dir"
check "PATH links removed" test ! -e "$bin_dir/bkengine" -a ! -e "$bin_dir/bkgui"
check "menu entry removed" test ! -e "$app_dir/backuputility.desktop"
check "icon removed" test ! -e "$icon_dir/backuputility.png"
check "desktop shortcut removed" test ! -e "$desk/backuputility.desktop"
check "timer removed" test ! -e "$unit_dir/bku-$job.timer"
check "job file kept" sh -c "./bkengine jobs show --job '$job'"

echo "=== cleanup"
./bkengine jobs remove --job "$job"
cd /
rm -rf "$work"
echo "=== done: $pass passed, $fail failed"
[ "$fail" -eq 0 ]

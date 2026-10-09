#!/bin/bash
# End-to-end check of the self-updater on Linux, against the real GitHub release.
#
# Builds an engine that believes it is version 0.0.9, puts it in a scratch folder
# next to a dummy bkgui, runs `bkengine update --yes` there, and checks that both
# binaries were replaced by the newest published release. Cargo.toml is restored
# afterwards. Needs a published release to exist, and a Rust toolchain.
#
#   bash scripts/updater_e2e.sh
# From Windows, inside the WSL build distro:
#   wsl -d Ubuntu-22.04 -u root -- bash -lc "cd /mnt/s/SERVICES/BackupUtility && CARGO_TARGET_DIR=/root/bku-target bash scripts/updater_e2e.sh"
set -u
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
export PATH="$HOME/.cargo/bin:$PATH"
target=${CARGO_TARGET_DIR:-"$root/target"}

cp Cargo.toml /tmp/Cargo.toml.updater-e2e
trap 'cp /tmp/Cargo.toml.updater-e2e Cargo.toml; git checkout -- Cargo.toml Cargo.lock 2>/dev/null; cargo build -p bkengine 2>&1 | grep -E "^error"; echo "restored; uncommitted changes: $(git status --short | wc -l)"' EXIT
sed -i 's/^version = "[0-9]*\.[0-9]*\.[0-9]*"/version = "0.0.9"/' Cargo.toml
echo "=== building a 0.0.9 engine"
cargo build -p bkengine 2>&1 | grep -E "^error"
scratch=/tmp/bku-updater-e2e
rm -rf "$scratch" && mkdir -p "$scratch"
cp "$target/debug/bkengine" "$scratch/bkengine"
printf dummy > "$scratch/bkgui" && chmod +x "$scratch/bkgui"
cd "$scratch"
echo "=== before:"; ./bkengine version | head -1
echo "=== update --check"; ./bkengine update --check
echo "=== update --yes"; ./bkengine update --yes; echo "exit: $?"
echo "=== after:"; ls -la "$scratch"; ./bkengine version | head -1; file ./bkgui | cut -c1-90
echo "=== a second check should find nothing newer and sweep leftovers"
./bkengine update --check; ls "$scratch"
cd "$root"

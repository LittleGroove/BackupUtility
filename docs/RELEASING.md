# Releasing

How a change gets from this folder to every install.

```text
  edit code ──> NEXT_RELEASE.md ──> tools/build_release.py ──> STAGING/vX.Y.Z/ ──> test by hand
                                                                                      │
          installs see it ◀── GitHub release vX.Y.Z ◀── tools/publish_release.py ◀────┘
```

The in-app updater reads the GitHub releases list, so **publishing is live**:
the moment `publish_release.py` finishes, every install that checks will be
offered the new version. Everything before that step is private to this
machine.

## One-time setup

- Rust (`rustup`) with the MSVC toolchain, and Python 3.9+ on the PATH.
- The GitHub CLI, signed in: `gh auth login`.
- For Linux builds without waiting on CI, a WSL distro with cargo and the GTK
  headers. Ubuntu 22.04 is what the release binaries are built against (its
  glibc runs on SteamOS 3.5+):

  ```powershell
  wsl --install -d Ubuntu-22.04 --no-launch
  wsl -d Ubuntu-22.04 -u root -- bash -lc "apt-get update && apt-get install -y build-essential pkg-config curl git libgtk-3-dev libxkbcommon-dev libwayland-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev && curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c rustfmt -c clippy"
  ```

  Without this, `build_release.py` dispatches the `build` workflow on GitHub
  Actions and downloads its Linux artifact, which takes a few minutes longer
  and needs the code pushed first.

## Each release

1. **Write the notes.** Add what changed to `NEXT_RELEASE.md`, under Added /
   Fixed / Changed, for the person using the tool.

2. **Build and stage.**

   ```powershell
   python tools/build_release.py --bump patch      # or --bump minor / --bump major
   ```

   This raises the version in `Cargo.toml`, commits the bump, runs the tests,
   builds release binaries for Windows (locally) and Linux (WSL or CI), and
   lays everything out under `STAGING/vX.Y.Z/`. `STAGING/` is ignored by git.

3. **Test from the staging folder.** `STAGING\vX.Y.Z\windows-x86_64\` is a
   complete install: double-click `bkgui.exe` there (it asks for elevation),
   open a job, run a dry run, check `bkengine.exe version` prints the new
   number. For Linux, copy `linux-x86_64/` to the machine and run
   `sh install.sh`, or test inside WSL:

   ```powershell
   wsl -d Ubuntu-22.04 -- bash -lc "cd /mnt/s/SERVICES/BackupUtility/STAGING/vX.Y.Z/linux-x86_64 && ./bkengine version && ./bkengine drives"
   ```

4. **Publish.**

   ```powershell
   python tools/publish_release.py            # stable
   python tools/publish_release.py --beta     # pre-release: only opted-in installs see it
   python tools/publish_release.py --dry-run  # show what would happen
   ```

   The script refuses to publish if the tree is dirty, if HEAD is not the
   commit the staging folder was built from, or if the tag already exists.
   It pushes `main`, creates the tag, creates the release with the assets and
   notes, and (for a stable release) resets `NEXT_RELEASE.md` and pushes that.

5. **Check the updater.** On any install of an older version:

   ```powershell
   bkengine update --check
   ```

   Or run the end-to-end scripts, which build a 0.0.9 engine in a scratch
   folder and let it update itself from the release you just published:

   ```powershell
   powershell -ExecutionPolicy Bypass -File scripts\updater_e2e.ps1
   wsl -d Ubuntu-22.04 -u root -- bash -lc "cd /mnt/s/SERVICES/BackupUtility && CARGO_TARGET_DIR=/root/bku-target bash scripts/updater_e2e.sh"
   ```

## What a release contains

| Asset | Who uses it |
|---|---|
| `BackupUtility-vX.Y.Z-windows-x86_64.zip` | a person installing on Windows: unzip anywhere, run `bkgui.exe` |
| `BackupUtility-vX.Y.Z-linux-x86_64.tar.gz` | a person installing on Linux: untar, `sh install.sh` |
| `bkengine-windows-x86_64.exe`, `bkgui-windows-x86_64.exe` | the updater on Windows |
| `bkengine-linux-x86_64`, `bkgui-linux-x86_64` | the updater on Linux |
| `SHA256SUMS` | the updater verifies every download against it before installing |

The updater swaps binaries in the folder the running program lives in and
touches nothing else. A scheduled job that is running at that moment keeps
running from the renamed old binary; the next run uses the new one.

## Channels

One version counter. A beta is the same build published as a pre-release with
a `-beta` tag; GitHub leaves pre-releases out of "latest", and the updater
ignores them unless the user ticks **include beta versions** in Settings. A
beta user still gets a stable release when it is the newer one, and turning
the setting off never downgrades anyone.

To ship the same version to both channels, publish twice: stable, then
`--beta`.

## If something goes wrong

- **A bad release went out.** Delete it (`gh release delete vX.Y.Z --yes
  --cleanup-tag`); installs that already updated keep working, and the next
  check sees the previous release as the newest. Then bump and publish a fix.
- **CI failed on Linux but Windows is fine.** Fix the Linux problem (the
  `linux` job's log says what), push, rebuild. Publishing with
  `--allow-missing-linux` is possible but leaves Linux installs on the old
  version until a release with Linux assets appears.
- **The staging folder is from an older commit.** The publisher refuses, by
  design: what was tested is what ships. Rebuild.

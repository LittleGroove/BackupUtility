# BackupUtility

Point it at a path, and it keeps that path mirrored somewhere else.

Usually that means a whole drive to its spare, sometimes a folder to a folder.
Runs are resumable, scheduled runs work with nobody logged in, and for a pair of
drives you want kept structurally identical there is an optional pass that
aligns their physical layouts.

```powershell
bkengine mirror --source D:\ --dest E:\
```

## What it does

| | |
|---|---|
| **True file copy** | data, all three timestamps, attributes, NTFS security descriptors, alternate data streams, sparse regions, hard links, junctions and symlinks |
| **Save states** | a run checkpoints as it goes; a crash, a reboot or Ctrl+C resumes from where it stopped instead of restarting |
| **Run logs** | every run leaves a log on disk, so a scheduled job that failed at 3am can actually be diagnosed |
| **Verification** | compare the two trees on demand, shallow or by hashing every byte — the check that answers "is the spare still good?" |
| **Scheduling** | Task Scheduler XML on Windows, systemd timers on Linux — with the settings that actually matter (catch up a missed run, do not die on battery, no execution time limit) |
| **Disk optimization** | *opt-in per job.* Detects HDD vs SSD and does the right thing — defragment spinning disks, retrim solid state, never the other way round. Media and capacity are read without needing Administrator |
| **Layout mirroring** | *opt-in per job.* Places files on the spare at the same physical cluster positions they occupy on the source |
| **Block-level imaging** | the original pipeline: deduplicated, changed-block volume images with restore and clone. Quiesced devices only — see below |

## Why Rust

C++ throughput with memory safety. For a tool that writes raw bytes to disk
sectors, the failure mode that matters is *silent backup corruption*, and Rust
removes the whole class of memory bugs that cause it — with no GC pauses during
a multi-hour copy.

## Quick start

```powershell
# what have we got?
bkengine drives

# see what a job would do, without doing it
bkengine mirror --source D:\Photos --dest E:\Backup\Photos --dry-run

# do it, verifying every byte written
bkengine mirror --source D:\Photos --dest E:\Backup\Photos --verify

# save it as a nightly job and register the scheduled task
bkengine jobs add --name photos --source D:\Photos --dest E:\Backup\Photos `
                  --mode attic --every daily --at 03:00
bkengine schedule install --job photos

# check on it later
bkengine status --job photos
bkengine logs   --job photos --show

# and prove the spare still matches, months on
bkengine verify --job photos --deep
```

Or run `bkgui` for the same thing with a window.

## The mirror

### What happens to files the source no longer has

Chosen per job, because the right answer differs by job:

| Mode | Behaviour |
|---|---|
| `additive` *(default)* | leave them. The destination only ever grows. |
| `attic` | move them to `<dest>\.bku-attic\<timestamp>\`. Reversible. |
| `delete` | remove them, so the two trees match exactly. |

Attic folders are pruned so they cannot grow without bound — by default the
last 10 runs are kept, with optional age and size limits
(`--attic-keep`, `--attic-days`). Pruning only ever touches folders this tool
created and named, never a directory someone else put there, and it never
removes the newest one: a policy that could empty the attic would discard the
only copy of a file the source had just lost.

### Deciding what changed

`--compare size-and-time` (the default) is what makes a nightly job viable: an
unchanged 8 TB drive costs a scan, not a re-transfer. `--compare content`
hashes both sides, which catches a file rewritten in place with its timestamp
restored, at the cost of reading everything on both drives.

The timestamp tolerance comes from the **destination** filesystem, because it is
the one that has to store what we write: exact on NTFS and ReFS, 10 ms on exFAT,
2 seconds on FAT32. Without that, an exFAT destination would re-copy the entire
tree every single night.

### Save states

A run writes its plan to `%PROGRAMDATA%\BackupUtility\state\<job>\` and appends a
4-byte record per completed action. Kill it at hour six of eight and the next run
picks up where it stopped.

The journal is append-only rather than rewritten, so a crash can only ever damage
the *last* record — and because every record is a fixed 4 bytes, a torn tail is
detectable and discarded. Worst case a handful of actions are redone, and every
action is idempotent.

Files are written to a `.bku-part` temporary and atomically renamed once complete
and their metadata is set. An interrupted copy leaves a temp file, never a
truncated file wearing the real name — which matters, because a half-written file
at the right path is one a user would later restore from and trust.

Alongside the state, every run writes `logs/<timestamp>.log`: the plan, each
failure, and the final report. A scheduled run has no console and Task Scheduler
keeps only the exit code, so without this there is no way to find out what the
3am job actually did. The last 30 are kept.

### Verification

```powershell
bkengine verify --job photos          # size and time — fast enough to run nightly
bkengine verify --job photos --deep   # hash every byte on both drives
```

Shallow catches truncation, missing files and anything that changed size. Deep
is the only check that catches silent corruption — a file with the right length
and timestamp but the wrong bytes — and it reads both drives in full. Exits
non-zero on any difference, so it can gate a scheduled task.

### Guards

Refused before anything is written:

- a destination **inside** the source (it would copy the backup into itself until
  the drive filled), or a source inside the destination
- a destination that plainly cannot hold the copy. The plan knows what it needs
  and the volume knows what it has, so an 8 TB mirror onto a too-small spare
  fails in milliseconds instead of eight hours in. Space that removals will free
  counts as available, so a delete-mode job swapping one large file for another
  is not refused for no reason.
- following a junction that points at its own ancestor — links are re-created,
  never descended into, unless you ask
- reading a OneDrive placeholder, which would silently pull terabytes down over
  the network. Skipped by default.

Reparse points that are *not* links — Data Deduplication, WOF-compressed system
files, cloud placeholders — are read as ordinary files. Treating them as links
would back up a stub instead of the data.

## Disk optimization (opt-in)

Most jobs are a plain copy and never touch this. For a pair of drives meant to
stay identical:

```powershell
bkengine jobs add --name spare --source D:\ --dest E:\ --identical-layout
```

### Media detection gates everything

Defragmenting an SSD rewrites tens of gigabytes for no benefit and costs write
endurance, so the operation is chosen from the drive, not from a flag:

| Media | Operation |
|---|---|
| spinning disk | defragment (`defrag /D`) |
| solid state + TRIM | retrim (`defrag /L`) |
| solid state, no TRIM | analyze only |
| could not tell | `defrag /O` — let Windows decide |

A USB enclosure often will not answer the seek-penalty query, so "unknown" is
never assumed to be spinning. **Asking for a defrag on an SSD is refused**, not
obeyed; the whole point of the detection is to stop that happening by accident.

The general defragmentation case is handed to Windows' own `defrag.exe`, which is
better at it than anything worth writing here. Fragmentation *measurement* is
ours, computed from cluster maps rather than by parsing `defrag /A` output —
that text is localized, so parsing it would silently return nothing on a
non-English system.

### Layout mirroring

`FSCTL_MOVE_FILE` — the interface every defragmenter uses — relocates a run of a
file's clusters to a chosen physical position, online, transactionally, with the
volume mounted.

Two drives with matching layouts stay matched: the read pattern that streams well
on the source streams identically on the spare, and as the drives fill, the free
space left behind is the same shape on both, so the divergence never compounds.

```powershell
bkengine layout --source D:\ --dest E:\           # measure
bkengine layout --source D:\ --dest E:\ --apply   # align
```

A file's target position is almost always occupied by something else, so the
pass also has to clear the way: it indexes every file on the destination volume
by the clusters it holds, then sweeps placements low-to-high while evacuating
occupants to free space up high. The two never chase each other, so nothing
already positioned gets displaced by a later eviction. Without that step nothing
ever moves on a real volume: without it the pass reports "0 files moved, every
run blocked". With it, the VHD harness goes from 0% to somewhere between 50% and
70% in a single pass — the figure varies run to run with how the target's free
space happens to fall, and runs that cannot place everything say so rather than
rounding up.

Guarded heavily, because every move is a real write: NTFS only, Administrator
only, and a cluster range is never targeted unless the volume bitmap says it is
free. The free map is updated as the pass proceeds, so two files can never be
sent to the same clusters. Nothing here ever changes file contents — the worst
case of a failed move is that the file stays where it was, which the report
counts as a blocked run.

## Scheduling

`schtasks /SC DAILY` cannot express the settings that decide whether an overnight
backup is actually useful, so the engine generates Task Scheduler XML instead:

- **`StartWhenAvailable`** — run late if the machine was off at 3am. Without it a
  server that was down overnight silently skips that night's backup.
- **`DisallowStartIfOnBatteries` / `StopIfGoingOnBatteries`** — both default to
  *true*, so on a laptop a scheduled backup never runs, and one already running is
  killed the moment the charger comes out.
- **`ExecutionTimeLimit`** — defaults to 72 hours; a multi-terabyte mirror should
  not be shot in the head partway.
- **`MultipleInstancesPolicy`** — stops a slow nightly run colliding with the next.

`RunLevel=HighestAvailable` with a SYSTEM principal means the task runs elevated
at its trigger time with no interactive UAC prompt.

## Architecture

| Crate | Kind | Role |
|---|---|---|
| `bkcore` | lib | everything real: scan, plan, copy, save state, disk optimization, block imaging |
| `bkengine` | bin | the CLI, and what the scheduler runs |
| `bkgui` | bin | egui desktop app; launches elevated via an embedded manifest |

Inside `bkcore`:

```text
file/    path  meta  filter  walk  plan  state  copy  mirror  sys
opt/     media  optimize  extents  layout
         chunk  manifest  store  backup  apply  diskio  vss  usn   (block level)
```

The split exists so the interesting decisions — what to copy, what to delete,
what counts as unchanged, what order things must happen in — are pure functions
over data in `plan`, testable in milliseconds, while the messy platform work is
isolated in `copy` and `sys`.

### Ordering is a correctness property

The plan is emitted in phases, and each exists for a concrete reason:

1. **Removals first** — frees space before writing, and clears wrong-kind entries
   out of the way. Deepest-first, because a directory will not delete until empty.
2. **Directories**, parents before children.
3. **Content**, with hard-link primaries before the extra names that link to them.
4. **Directory metadata last** — the subtle one. Writing a file into a directory
   updates that directory's modified time, so restoring timestamps *before*
   filling it destroys them, and the next run sees every directory as changed.

### Platform

Windows is what is implemented and tested. The seam is clean: `path`, `meta`,
`filter`, `plan`, `state` and `mirror` carry no platform types, and `sys`, `copy`
and `opt` are `#[cfg]`-gated.

The Linux building blocks are real, not stubs — `SEEK_DATA`/`SEEK_HOLE` sparse
copying, `utimensat` nanosecond timestamps, `/sys/block/*/queue/rotational` media
detection, `fstrim`, systemd timer and crontab generation — but they are
**untested**, because there is no Linux here to test them on. Two gaps are
honestly reported rather than faked: extent maps need `FS_IOC_FIEMAP`, and
cluster placement has no ext4 equivalent (`EXT4_IOC_MOVE_EXT` targets a donor
file, not an absolute position).

## Building

Needs Rust with the MSVC toolchain and "Desktop development with C++".

```powershell
cargo build --workspace
cargo test  --workspace      # 223 tests
```

### Running the GUI

`cargo run -p bkgui` does **not** work, and neither does launching the binary
from Git Bash. The embedded `requireAdministrator` manifest makes a plain
`CreateProcess` fail with error 740 (`ERROR_ELEVATION_REQUIRED`) — the launch
has to go through the shell so Windows can raise the UAC prompt:

```powershell
Start-Process .\target\debug\bkgui.exe -Verb RunAs
```

Double-clicking `target\debug\bkgui.exe` in Explorer works too. From an already
elevated prompt it runs directly.

Debug builds keep the console window attached, so panics and `tracing` output
are visible while testing; release builds hide it.

## Testing

`cargo test` covers the engine: 189 unit tests plus 34 integration tests that
mirror real trees on disk and assert the destination matches, that a second run
copies nothing, that an interrupted run resumes without redoing finished work,
that read-only files can be replaced, that a cancelled copy leaves no file at the
real path, that the attic prunes to its limit without ever emptying itself, that
every run leaves a log (including one that refused to start), and that hashing
catches a corrupted destination byte that size-and-time cannot see.

Two elevated harnesses cover what needs real disks. Both create disposable VHDs
and refuse to touch any disk whose `BusType` is not `File Backed Virtual`, so
they cannot write a physical drive:

```powershell
# file mirror + layout alignment + data integrity after moving clusters
powershell -ExecutionPolicy Bypass -File scripts\vhd_mirror_test.ps1 `
  -EnginePath .\target\debug\bkengine.exe -WorkDir $env:TEMP\bkmirror `
  -LogPath $env:TEMP\bkmirror_result.log

# block-level disk-to-disk cloning
powershell -ExecutionPolicy Bypass -File scripts\vhd_clone_test.ps1 `
  -EnginePath .\target\debug\bkengine.exe -WorkDir $env:TEMP\bkvhdtest `
  -LogPath $env:TEMP\bkvhd_result.log
```

## Block-level imaging

The original pipeline, kept as the secondary mode for bare-metal restore. A VSS
snapshot is read, split into 1 MiB blocks, each BLAKE3-hashed, and only blocks
whose hash is new are stored — content-addressed, so deduplication and the
changed-block delta fall out for free, with no kernel CBT driver and no code
signing.

**Imaging a live volume is refused.** A consistent snapshot needs VSS, which is
not implemented, and reading a mounted volume while Windows writes to it
produces a torn image that may not even mount. Rather than hand back something
that looks fine and will not restore, the engine declines and points at the
file-level mirror. `--allow-torn` overrides it for a genuinely quiesced device
(an offline disk, or a VHD nothing is writing to).

```powershell
bkengine backup  --job system --source C: --store E:\Images
bkengine verify  --job system
bkengine restore --job system --target D:\system.img
bkengine clone   --job system --target \\.\PhysicalDrive9 --delta --confirm
```

Restore and clone share one apply loop, differing only in the target: a file for
imaging, a raw device for cloning. With `--delta` a re-clone reads the target
first and writes only the blocks that differ.

## Status

- [x] file mirror: scan, plan, copy, commit — proven end to end
- [x] full NTFS fidelity: ACLs, streams, sparse, hard links, junctions, long paths
- [x] save states: resumable, crash-safe, torn-record tolerant
- [x] per-run logs, pruned to the last 30
- [x] attic / delete / additive modes, with attic retention
- [x] free-space preflight
- [x] tree verification, shallow and by hash
- [x] media detection, media-appropriate optimization, refusal to defrag SSDs
- [x] fragmentation analysis from cluster maps
- [x] layout mirroring via `FSCTL_MOVE_FILE`, with eviction to clear targets
- [x] Task Scheduler XML + systemd timers + crontab
- [x] GUI: job editor, live progress, drive list, run and stop
- [x] block-level image, restore, clone, validated against VHDs
- [x] validated on real NTFS volumes: mirror, deep verification, delete mode, and
      a layout pass that took similarity from 0% to 50-70% with every file still
      hashing identically afterwards (`scripts\vhd_mirror_test.ps1`)
- [ ] Windows 11 (developed and tested on Windows 10 19045)
- [ ] `vss`: live-volume snapshot, so `C:` can be imaged while running
- [ ] `usn`: change-journal incremental, to skip the full scan
- [ ] Linux: `FS_IOC_FIEMAP` extents, and anything at all actually tested

# Release notes — next release

<!--
  Keep this file current as changes land. Everything below this comment becomes
  the body of the GitHub release when tools/publish_release.py runs, and a
  stable publish resets the file to this template. Write for the person using
  the tool, not for the developer: what changed for them, and why it matters.
-->

First release.

BackupUtility mirrors a path to another drive on a schedule. Point a job at a
source and a destination, choose what happens to files the source no longer
has (keep them, move them to a dated attic, or delete them so the two trees
match exactly), and let the scheduler run it.

## Added

- **Windows and Linux builds.** `bkgui` is the window, `bkengine` the command
  line and what the scheduler runs. The Linux build runs on SteamOS without
  root: jobs schedule as user-level systemd timers.
- **Built-in updates.** The window checks this releases page when it starts
  and offers to install a newer version in place; `bkengine update` does the
  same from a terminal. Downloads are checked against `SHA256SUMS` before
  anything is replaced. Betas are opt-in under Settings.
- **File mirror** with full NTFS fidelity: timestamps, attributes, security
  descriptors, alternate data streams, sparse files, hard links, junctions and
  long paths.
- **Three modes for vanished files:** additive (never delete), attic (move to
  a dated folder, pruned by count, age or size) and delete (exact copy).
- **Save states.** A run that is interrupted — crash, reboot, Ctrl+C, Stop —
  resumes where it stopped instead of starting over.
- **Per-run logs**, kept for the last 30 runs, viewable from the window.
- **Verification**, shallow (size and time) or deep (hash every byte).
- **Scheduling** through Task Scheduler on Windows and systemd timers on
  Linux, with the settings that matter: catch up a missed run, no execution
  time limit, run with nobody logged in.
- **Disk optimization**, opt-in per job: defragment spinning disks, retrim
  SSDs (never the other way round), and align the spare's physical layout to
  the source.
- **Drive list** with media type, filesystem and free space on both platforms.

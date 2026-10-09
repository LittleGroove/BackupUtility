# Release notes — next release

<!--
  Keep this file current as changes land. Everything below this comment becomes
  the body of the GitHub release when tools/publish_release.py runs, and a
  stable publish resets the file to this template. Write for the person using
  the tool, not for the developer: what changed for them, and why it matters.
-->

## Added

- The program now installs itself. Run `bkgui` from the unzipped folder and it
  offers to copy itself to its home (`C:\Program Files\BackupUtility` on
  Windows, `~/.local/share/BackupUtility` on Linux), add a Start Menu or
  application-menu entry and, if ticked, a desktop shortcut, then starts the
  installed copy and removes the one you unzipped. Scheduled jobs are
  re-pointed at the installed engine, so an existing unzipped-anywhere copy
  can simply update: its next start finishes the move. If a newer version is
  already installed, the copy you started offers to launch that one instead.
- An uninstaller: Settings > Apps on Windows, `bkengine uninstall` on either
  platform. It removes the program, its shortcuts and its scheduled tasks, and
  keeps jobs, settings and run logs unless asked to delete them.
- `bkengine install` does the install from a terminal, and `bkengine version`
  now says where the home is and whether this copy is the installed one.

## Fixed

## Changed

- On Linux, `install.sh` now runs `bkengine install`, so the script and the
  window produce the same install.
- `bkengine update` on Windows needs an elevated terminal, because the
  installed program lives under Program Files.

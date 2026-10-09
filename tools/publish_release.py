#!/usr/bin/env python3
"""Publish a staged build (from tools/build_release.py) to GitHub.

    main branch   <- the source, pushed as it is (this folder is the repository)
    tag vX.Y.Z    <- created at the commit the staging folder was built from
    release       <- STAGING/vX.Y.Z/release/* attached as assets; the body is
                     STAGING/vX.Y.Z/RELEASE_NOTES.md

The in-app updater reads the releases list, so a published release is live
for every install the moment this script finishes. Betas (`--beta`) are tagged
`vX.Y.Z-beta` and marked as a pre-release, which keeps them invisible to anyone
who has not opted in.

    python tools/publish_release.py                 newest STAGING/v* folder
    python tools/publish_release.py --version 0.1.1
    python tools/publish_release.py --beta          pre-release on the same version
    python tools/publish_release.py --dry-run       print the commands, change nothing
    python tools/publish_release.py --force-replace delete and recreate an existing release

After a stable publish, NEXT_RELEASE.md is reset for the next cycle and that
reset is committed and pushed.

Standard library only; needs git and the GitHub CLI (`gh auth login` once).
"""
import argparse
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STAGING = ROOT / "STAGING"
NOTES = ROOT / "NEXT_RELEASE.md"
REPO = "LittleGroove/BackupUtility"

NOTES_TEMPLATE = """# Release notes — next release

<!--
  Keep this file current as changes land. Everything below this comment becomes
  the body of the GitHub release when tools/publish_release.py runs, and a
  stable publish resets the file to this template. Write for the person using
  the tool, not for the developer: what changed for them, and why it matters.
-->

## Added

## Fixed

## Changed
"""

DRY = False


def say(msg):
    print(f"[publish] {msg}", flush=True)


def die(msg):
    print(f"[publish] error: {msg}", file=sys.stderr)
    sys.exit(1)


def run(cmd, check=True, capture=False, mutating=False):
    if DRY and mutating:
        say("would run: " + " ".join(map(str, cmd)))
        return subprocess.CompletedProcess(cmd, 0, "", "")
    res = subprocess.run(cmd, cwd=str(ROOT), text=True, capture_output=capture)
    if check and res.returncode != 0:
        if capture:
            sys.stderr.write(res.stdout or "")
            sys.stderr.write(res.stderr or "")
        die(f"command failed ({res.returncode}): {' '.join(map(str, cmd))}")
    return res


def git(*args, capture=True, check=True, mutating=False):
    return run(["git", *args], check=check, capture=capture, mutating=mutating)


def gh(*args, capture=True, check=True, mutating=False):
    return run(["gh", *args], check=check, capture=capture, mutating=mutating)


def newest_staged():
    if not STAGING.is_dir():
        die("STAGING/ does not exist; run tools/build_release.py first")
    found = []
    for d in STAGING.iterdir():
        m = re.fullmatch(r"v(\d+)\.(\d+)\.(\d+)", d.name)
        if d.is_dir() and m:
            found.append((tuple(int(x) for x in m.groups()), d.name[1:]))
    if not found:
        die("no STAGING/vX.Y.Z folders; run tools/build_release.py first")
    found.sort()
    return found[-1][1]


def main():
    global DRY
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--version", help="X.Y.Z to publish (default: newest staged)")
    ap.add_argument("--beta", action="store_true", help="publish as a pre-release tagged vX.Y.Z-beta")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--yes", "-y", action="store_true", help="skip the confirmation prompt")
    ap.add_argument("--force-replace", action="store_true", help="delete an existing release and tag of this name first")
    ap.add_argument("--allow-missing-linux", action="store_true", help="publish without Linux assets")
    ap.add_argument("--allow-commit-mismatch", action="store_true",
                    help="publish even if HEAD is not the commit the staging folder was built from")
    args = ap.parse_args()
    DRY = args.dry_run

    for tool in ("git", "gh"):
        if not shutil.which(tool):
            die(f"{tool} is not installed")
    gh("auth", "status")

    version = args.version or newest_staged()
    staged = STAGING / f"v{version}"
    rel_dir = staged / "release"
    info_path = staged / "BUILD_INFO.json"
    notes_path = staged / "RELEASE_NOTES.md"
    if not rel_dir.is_dir() or not info_path.exists():
        die(f"{staged} is not a complete staging folder")
    info = json.loads(info_path.read_text(encoding="utf-8"))
    assets = sorted(p for p in rel_dir.iterdir() if p.is_file())
    names = {p.name for p in assets}

    # -- preflight ----------------------------------------------------------
    if "SHA256SUMS" not in names:
        die("release/ has no SHA256SUMS")
    if not any(n.endswith("windows-x86_64.exe") for n in names):
        die("release/ has no Windows binaries")
    if not any(n == "bkengine-linux-x86_64" for n in names):
        if not args.allow_missing_linux:
            die("release/ has no Linux binaries; rebuild with Linux, or pass --allow-missing-linux")
        say("warning: no Linux assets — Linux installs will not see this release")
    if info.get("dirty"):
        say("warning: the staging folder was built from a dirty tree")

    head = git("rev-parse", "HEAD").stdout.strip()
    if head != info.get("commit") and not args.allow_commit_mismatch:
        die(f"HEAD is {head[:8]} but the staged build is from {info.get('commit', '?')[:8]}; "
            "rebuild, check out that commit, or pass --allow-commit-mismatch")
    dirty = git("status", "--porcelain", "--untracked-files=no").stdout.strip()
    if dirty:
        die("the working tree has uncommitted changes:\n" + dirty)
    branch = git("rev-parse", "--abbrev-ref", "HEAD").stdout.strip()

    tag = f"v{version}-beta" if args.beta else f"v{version}"
    title = f"BackupUtility {version}" + (" (beta)" if args.beta else "")
    exists_local = git("tag", "-l", tag).stdout.strip() == tag
    exists_remote = bool(git("ls-remote", "--tags", "origin", f"refs/tags/{tag}").stdout.strip())
    release_exists = gh("release", "view", tag, "--json", "url", check=False).returncode == 0
    if (exists_local or exists_remote or release_exists) and not args.force_replace:
        die(f"{tag} already exists (local: {exists_local}, remote: {exists_remote}, release: {release_exists}); "
            "bump the version, or pass --force-replace")

    body = notes_path.read_text(encoding="utf-8") if notes_path.exists() else f"BackupUtility {version}\n"

    say(f"publishing {title}")
    say(f"  tag        {tag} at {head[:8]} ({branch})")
    say(f"  assets     {len(assets)} files from {rel_dir}")
    for p in assets:
        say(f"             {p.name}")
    say(f"  notes      {len(body.splitlines())} lines")
    if not args.yes and not DRY:
        answer = input("continue? [y/N] ").strip().lower()
        if answer not in ("y", "yes"):
            die("aborted")

    # -- replace -------------------------------------------------------------
    if args.force_replace:
        if release_exists:
            gh("release", "delete", tag, "--yes", "--cleanup-tag", mutating=True)
        if exists_local:
            git("tag", "-d", tag, mutating=True)
        if exists_remote:
            git("push", "origin", f":refs/tags/{tag}", check=False, mutating=True)

    # -- push source, tag, release -------------------------------------------
    git("push", "origin", branch, capture=False, mutating=True)
    git("tag", "-a", tag, "-m", title, mutating=True)
    git("push", "origin", tag, capture=False, mutating=True)
    cmd = ["release", "create", tag, "--title", title, "--notes-file", str(notes_path if notes_path.exists() else "-"),
           "--verify-tag"]
    if args.beta:
        cmd.append("--prerelease")
    cmd += [str(p) for p in assets]
    gh(*cmd, capture=False, mutating=True)
    url = f"https://github.com/{REPO}/releases/tag/{tag}"
    say(f"published {url}")

    # -- reset the notes after a stable release -------------------------------
    if not args.beta and NOTES.exists():
        if DRY:
            say(f"would reset {NOTES.name} and push the reset")
        else:
            NOTES.write_text(NOTES_TEMPLATE, encoding="utf-8", newline="\n")
            git("add", str(NOTES))
            if git("diff", "--cached", "--quiet", check=False).returncode != 0:
                git("commit", "-q", "-m", f"Reset release notes after {tag}")
                git("push", "origin", branch, capture=False)
                say("reset NEXT_RELEASE.md for the next cycle")
    say("done")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Build a release into STAGING/v<X.Y.Z>/ — everything a release needs, laid out
so it can be tested by hand before tools/publish_release.py uploads it.

    STAGING/v0.1.0/
      windows-x86_64/   bkengine.exe  bkgui.exe  README.md  LICENSE     <- run these to test
      linux-x86_64/     bkengine  bkgui  install.sh  icon.png  README.md  LICENSE
      release/          exactly the files that get attached to the GitHub release:
                          BackupUtility-v0.1.0-windows-x86_64.zip
                          BackupUtility-v0.1.0-linux-x86_64.tar.gz
                          bkengine-windows-x86_64.exe  bkgui-windows-x86_64.exe
                          bkengine-linux-x86_64        bkgui-linux-x86_64
                          SHA256SUMS
      RELEASE_NOTES.md  the body of NEXT_RELEASE.md at build time
      BUILD_INFO.json   version, commit, how each platform was built

The Windows binaries are built locally. The Linux binaries come from WSL
(an Ubuntu distro with cargo and the GTK headers — see docs/RELEASING.md) or,
when WSL is not set up, from the GitHub Actions `build` workflow, which this
script triggers and waits for. On a Linux host the roles swap.

    python tools/build_release.py                    build the current version
    python tools/build_release.py --bump patch       0.1.0 -> 0.1.1, commit, build
    python tools/build_release.py --linux ci         force the CI path for Linux
    python tools/build_release.py --linux skip       Windows only (updater on Linux
                                                     will then ignore this release)

Standard library only; Python 3.9+.
"""
import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import time
import zipfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STAGING = ROOT / "STAGING"
CARGO_TOML = ROOT / "Cargo.toml"
NOTES = ROOT / "NEXT_RELEASE.md"
WORKFLOW = "build.yml"
BINARIES = ("bkengine", "bkgui")
IS_WINDOWS = platform.system() == "Windows"


# ── helpers ────────────────────────────────────────────────────────────────

def say(msg):
    print(f"[build] {msg}", flush=True)


def die(msg):
    print(f"[build] error: {msg}", file=sys.stderr)
    sys.exit(1)


def cargo_env():
    env = os.environ.copy()
    cargo_bin = Path.home() / ".cargo" / "bin"
    if cargo_bin.is_dir():
        env["PATH"] = str(cargo_bin) + os.pathsep + env.get("PATH", "")
    return env


def run(cmd, cwd=ROOT, check=True, capture=False, env=None):
    res = subprocess.run(cmd, cwd=str(cwd), text=True, capture_output=capture,
                         env=env or cargo_env())
    if check and res.returncode != 0:
        if capture:
            sys.stderr.write(res.stdout or "")
            sys.stderr.write(res.stderr or "")
        die(f"command failed ({res.returncode}): {' '.join(map(str, cmd))}")
    return res


def git(*args, capture=True, check=True):
    return run(["git", *args], capture=capture, check=check)


def current_version():
    text = CARGO_TOML.read_text(encoding="utf-8")
    m = re.search(r"^\[workspace\.package\]\s*$(.*?)^\[", text, re.M | re.S)
    block = m.group(1) if m else text
    v = re.search(r'^version\s*=\s*"(\d+\.\d+\.\d+)"', block, re.M)
    if not v:
        die("could not find the workspace version in Cargo.toml")
    return v.group(1)


def set_version(new):
    text = CARGO_TOML.read_text(encoding="utf-8")
    start = text.index("[workspace.package]")
    end_m = re.search(r"^\[", text[start + 1:], re.M)
    end = start + 1 + end_m.start() if end_m else len(text)
    block = text[start:end]
    block2, n = re.subn(r'^(version\s*=\s*")\d+\.\d+\.\d+(")', rf"\g<1>{new}\g<2>", block, count=1, flags=re.M)
    if n != 1:
        die("could not rewrite the workspace version")
    CARGO_TOML.write_text(text[:start] + block2 + text[end:], encoding="utf-8", newline="\n")


def bump(version, part):
    major, minor, patch = (int(x) for x in version.split("."))
    if part == "major":
        return f"{major + 1}.0.0"
    if part == "minor":
        return f"{major}.{minor + 1}.0"
    return f"{major}.{minor}.{patch + 1}"


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def notes_body():
    """Everything in NEXT_RELEASE.md below the HTML comment, or a placeholder."""
    if not NOTES.exists():
        return ""
    text = NOTES.read_text(encoding="utf-8")
    if "-->" in text:
        text = text.split("-->", 1)[1]
    text = text.strip()
    # Drop empty headings so the release body is not a list of bare "## Fixed".
    kept, pending = [], None
    for line in text.splitlines():
        if line.startswith("## "):
            pending = line
            continue
        if line.strip() and pending is not None:
            kept.append(pending)
            pending = None
        if line.strip() or (kept and kept[-1].strip()):
            kept.append(line)
    return "\n".join(kept).strip() + "\n"


# ── WSL ────────────────────────────────────────────────────────────────────

def wsl_distros():
    if not IS_WINDOWS or not shutil.which("wsl.exe"):
        return []
    res = subprocess.run(["wsl.exe", "--list", "--quiet"], capture_output=True)
    text = res.stdout.decode("utf-16-le", errors="ignore") if res.stdout else ""
    return [d.strip().strip("\x00") for d in text.splitlines() if d.strip().strip("\x00")]


def wsl_has_cargo(distro):
    res = subprocess.run(["wsl.exe", "-d", distro, "--", "sh", "-lc", "test -x \"$HOME/.cargo/bin/cargo\" && echo yes"],
                         capture_output=True, text=True)
    return "yes" in (res.stdout or "")


def pick_wsl_distro(preferred):
    distros = wsl_distros()
    if preferred:
        return preferred if preferred in distros and wsl_has_cargo(preferred) else None
    for d in distros:
        if d == "docker-desktop" or d.startswith("docker-desktop"):
            continue
        if wsl_has_cargo(d):
            return d
    return None


def wsl_path(distro, windows_path):
    """`S:\\x\\y` -> `/mnt/s/x/y`. Backslashes do not survive the wsl.exe command
    line, so the path is handed to wslpath with forward slashes, and the
    conventional mapping is the fallback for an unusual mount layout."""
    forward = str(windows_path).replace("\\", "/")
    res = subprocess.run(["wsl.exe", "-d", distro, "--", "wslpath", "-a", forward],
                         capture_output=True, text=True)
    out = (res.stdout or "").strip()
    if res.returncode == 0 and out.startswith("/"):
        return out
    drive, _, rest = forward.partition(":")
    if len(drive) == 1 and drive.isalpha():
        return f"/mnt/{drive.lower()}{rest if rest.startswith('/') else '/' + rest}".rstrip("/")
    die(f"cannot map {windows_path} into WSL: {res.stderr.strip()}")


def build_linux_in_wsl(distro, out_dir, skip_tests):
    repo = wsl_path(distro, ROOT)
    out = wsl_path(distro, out_dir)
    steps = [
        "set -e",
        f"cd '{repo}'",
        "export PATH=\"$HOME/.cargo/bin:$PATH\"",
        # A native target dir: building on /mnt/<drive> is several times slower.
        "export CARGO_TARGET_DIR=\"$HOME/bku-target\"",
        "" if skip_tests else "cargo test --workspace",
        "cargo build --release --workspace",
        f"mkdir -p '{out}'",
        *[f"cp \"$CARGO_TARGET_DIR/release/{b}\" '{out}/{b}'" for b in BINARIES],
        "rustc --version > '" + out + "/.rustc-version'",
    ]
    script = "\n".join(s for s in steps if s)
    say(f"building Linux binaries in WSL distro '{distro}'")
    res = subprocess.run(["wsl.exe", "-d", distro, "--", "bash", "-lc", script], text=True)
    if res.returncode != 0:
        die("the WSL build failed")


# ── GitHub Actions ─────────────────────────────────────────────────────────

def gh(*args, capture=True, check=True):
    if not shutil.which("gh"):
        die("the GitHub CLI (gh) is not installed; it is needed for the CI path and for publishing")
    return run(["gh", *args], capture=capture, check=check)


def build_linux_in_ci(out_dir):
    head = git("rev-parse", "HEAD").stdout.strip()
    branch = git("rev-parse", "--abbrev-ref", "HEAD").stdout.strip()
    # The workflow runs the pushed code, so HEAD must be on the remote.
    remote = git("ls-remote", "origin", f"refs/heads/{branch}").stdout.split()
    if not remote or remote[0] != head:
        die(f"HEAD ({head[:8]}) is not pushed to origin/{branch}; push first (the CI build runs what is on GitHub)")
    say(f"dispatching the '{WORKFLOW}' workflow on {branch} ({head[:8]})")
    started = time.time()
    gh("workflow", "run", WORKFLOW, "--ref", branch)
    run_id = None
    deadline = time.time() + 60 * 40
    while time.time() < deadline:
        time.sleep(10)
        res = gh("run", "list", "--workflow", WORKFLOW, "--event", "workflow_dispatch",
                 "--json", "databaseId,headSha,status,conclusion,createdAt", "--limit", "10")
        runs = json.loads(res.stdout or "[]")
        for r in runs:
            created = datetime.fromisoformat(r["createdAt"].replace("Z", "+00:00")).timestamp()
            if r["headSha"] == head and created >= started - 120:
                run_id = r["databaseId"]
                if r["status"] == "completed":
                    if r["conclusion"] != "success":
                        die(f"CI run {run_id} finished with: {r['conclusion']} (gh run view {run_id})")
                    out_dir.mkdir(parents=True, exist_ok=True)
                    say(f"CI run {run_id} succeeded; downloading linux-x86_64")
                    gh("run", "download", str(run_id), "-n", "linux-x86_64", "-D", str(out_dir), capture=False)
                    for b in BINARIES:
                        os.chmod(out_dir / b, 0o755) if not IS_WINDOWS else None
                    return
                break
        say(f"waiting for CI run {run_id or '(not listed yet)'}...")
    die("timed out waiting for the CI build")


# ── packaging ──────────────────────────────────────────────────────────────

def stage_common(dest):
    shutil.copy2(ROOT / "README.md", dest / "README.md")
    if (ROOT / "LICENSE").exists():
        shutil.copy2(ROOT / "LICENSE", dest / "LICENSE")


def zip_dir(src_dir, zip_path, top):
    with zipfile.ZipFile(zip_path, "w", zipfile.ZIP_DEFLATED) as z:
        for p in sorted(src_dir.rglob("*")):
            if p.is_file() and not p.name.startswith("."):
                z.write(p, f"{top}/{p.relative_to(src_dir).as_posix()}")


def tar_dir(src_dir, tar_path, top):
    def fix(info):
        info.uid = info.gid = 0
        info.uname = info.gname = "root"
        if info.name.endswith(("/bkengine", "/bkgui", "/install.sh")):
            info.mode = 0o755
        elif info.isfile():
            info.mode = 0o644
        return info
    with tarfile.open(tar_path, "w:gz") as t:
        for p in sorted(src_dir.rglob("*")):
            if p.is_file() and not p.name.startswith("."):
                t.add(p, arcname=f"{top}/{p.relative_to(src_dir).as_posix()}", filter=fix)


# ── main ───────────────────────────────────────────────────────────────────

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    g = ap.add_mutually_exclusive_group()
    g.add_argument("--bump", choices=["patch", "minor", "major"], help="raise the version and commit the bump first")
    g.add_argument("--version", help="set an explicit X.Y.Z and commit it first")
    ap.add_argument("--linux", choices=["auto", "wsl", "ci", "skip"], default="auto",
                    help="where the Linux binaries come from (default: WSL if it has cargo, else CI)")
    ap.add_argument("--wsl-distro", help="the WSL distro to build in (default: the first one with cargo)")
    ap.add_argument("--skip-tests", action="store_true", help="do not run cargo test before building")
    ap.add_argument("--allow-dirty", action="store_true", help="build even with uncommitted changes")
    ap.add_argument("--no-commit", action="store_true", help="with --bump/--version: edit Cargo.toml but do not commit")
    args = ap.parse_args()

    os.chdir(ROOT)
    if not shutil.which("git"):
        die("git is not installed")

    dirty = git("status", "--porcelain", "--untracked-files=no").stdout.strip()
    if dirty and not args.allow_dirty:
        die("the working tree has uncommitted changes; commit them (or pass --allow-dirty):\n" + dirty)

    # 1. Version
    version = current_version()
    if args.bump or args.version:
        new = bump(version, args.bump) if args.bump else args.version
        if not re.fullmatch(r"\d+\.\d+\.\d+", new):
            die(f"'{new}' is not X.Y.Z")
        say(f"version {version} -> {new}")
        set_version(new)
        run(["cargo", "update", "--workspace", "--offline"], check=False)
        if not args.no_commit:
            git("add", "Cargo.toml", "Cargo.lock")
            git("commit", "-q", "-m", f"Bump version to {new}")
            say(f"committed the bump ({git('rev-parse', '--short', 'HEAD').stdout.strip()})")
        version = new
    tag = f"v{version}"
    commit = git("rev-parse", "HEAD").stdout.strip()

    out = STAGING / tag
    if out.exists():
        say(f"replacing existing {out.relative_to(ROOT)}")
        shutil.rmtree(out)
    win_dir = out / "windows-x86_64"
    lin_dir = out / "linux-x86_64"
    rel_dir = out / "release"
    for d in (win_dir, lin_dir, rel_dir):
        d.mkdir(parents=True, exist_ok=True)

    info = {
        "version": version,
        "tag": tag,
        "commit": commit,
        "dirty": bool(dirty),
        "built_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "host": platform.platform(),
        "windows": None,
        "linux": None,
    }

    # 2. The host platform's binaries, built locally.
    local_dir, local_key = (win_dir, "windows") if IS_WINDOWS else (lin_dir, "linux")
    if not args.skip_tests:
        say("cargo test --workspace")
        run(["cargo", "test", "--workspace"])
    say("cargo build --release --workspace")
    run(["cargo", "build", "--release", "--workspace"])
    rel = ROOT / "target" / "release"
    for b in BINARIES:
        name = b + (".exe" if IS_WINDOWS else "")
        src = rel / name
        if not src.exists():
            die(f"{src} was not built")
        shutil.copy2(src, local_dir / name)
    rustc = run(["rustc", "--version"], capture=True).stdout.strip()
    info[local_key] = {"source": "local", "rustc": rustc}

    # 3. The other platform.
    if IS_WINDOWS:
        how = args.linux
        distro = None
        if how in ("auto", "wsl"):
            distro = pick_wsl_distro(args.wsl_distro)
            if how == "wsl" and not distro:
                die("no WSL distro with cargo found; see docs/RELEASING.md for the one-time setup")
            how = "wsl" if distro else ("ci" if how == "auto" else how)
        if how == "wsl":
            build_linux_in_wsl(distro, lin_dir, args.skip_tests)
            rv = (lin_dir / ".rustc-version")
            info["linux"] = {"source": f"wsl:{distro}", "rustc": rv.read_text().strip() if rv.exists() else ""}
            rv.unlink(missing_ok=True)
        elif how == "ci":
            build_linux_in_ci(lin_dir)
            info["linux"] = {"source": "github-actions", "rustc": ""}
        else:
            say("skipping the Linux build")
            shutil.rmtree(lin_dir)
    else:
        say("Windows binaries are not built on a Linux host; use the CI artifact or build on Windows")
        shutil.rmtree(win_dir)

    # 4. Lay out the install folders and the release assets.
    if win_dir.exists():
        stage_common(win_dir)
        for b in BINARIES:
            shutil.copy2(win_dir / f"{b}.exe", rel_dir / f"{b}-windows-x86_64.exe")
        zip_dir(win_dir, rel_dir / f"BackupUtility-{tag}-windows-x86_64.zip", f"BackupUtility-{tag}-windows-x86_64")
    if lin_dir.exists():
        stage_common(lin_dir)
        shutil.copy2(ROOT / "packaging" / "linux" / "install.sh", lin_dir / "install.sh")
        shutil.copy2(ROOT / "assets" / "icon.png", lin_dir / "icon.png")
        for b in BINARIES:
            shutil.copy2(lin_dir / b, rel_dir / f"{b}-linux-x86_64")
        tar_dir(lin_dir, rel_dir / f"BackupUtility-{tag}-linux-x86_64.tar.gz", f"BackupUtility-{tag}-linux-x86_64")

    sums = []
    for p in sorted(rel_dir.iterdir()):
        if p.name != "SHA256SUMS":
            sums.append(f"{sha256(p)}  {p.name}")
    (rel_dir / "SHA256SUMS").write_text("\n".join(sums) + "\n", encoding="utf-8", newline="\n")

    body = notes_body()
    (out / "RELEASE_NOTES.md").write_text(body or f"BackupUtility {tag}\n", encoding="utf-8", newline="\n")
    (out / "BUILD_INFO.json").write_text(json.dumps(info, indent=2) + "\n", encoding="utf-8", newline="\n")

    say(f"staged {tag} at {out}")
    for p in sorted(rel_dir.iterdir()):
        say(f"  release/{p.name:<48} {p.stat().st_size:>12,} bytes")
    say("test it from the staging folder, then: python tools/publish_release.py")
    if not body:
        say("note: NEXT_RELEASE.md is empty; the release body will be a placeholder")


if __name__ == "__main__":
    main()

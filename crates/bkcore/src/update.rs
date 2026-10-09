//! Self-update from the GitHub Releases page.
//!
//! ## What a release looks like
//! Every release at <https://github.com/LittleGroove/BackupUtility/releases>
//! is tagged `vX.Y.Z` (stable) or `vX.Y.Z-beta` (a GitHub *pre-release*) and
//! carries, for each platform:
//!
//! ```text
//!   bkengine-windows-x86_64.exe    bkgui-windows-x86_64.exe
//!   bkengine-linux-x86_64          bkgui-linux-x86_64
//!   BackupUtility-vX.Y.Z-windows-x86_64.zip        (bundle, for a first install)
//!   BackupUtility-vX.Y.Z-linux-x86_64.tar.gz
//!   SHA256SUMS
//! ```
//!
//! The updater downloads the two bare binaries for its platform, checks each
//! against `SHA256SUMS` (which travelled over the same HTTPS connection, so
//! this catches a torn download, not a hostile server), and swaps them into
//! the directory the running executable lives in. Nothing else — jobs, state,
//! settings — is touched, and no installer runs.
//!
//! ## Swapping a binary that is running
//! A running executable cannot be overwritten, but on both Windows and Linux
//! it *can* be renamed. The engine, which a scheduled task may be running at
//! that very moment, is renamed to `bkengine.exe.old` and the new file takes
//! its name; the job in flight keeps running from the renamed file. The
//! executable doing the updating replaces itself the same way, through the
//! `self-replace` crate. Leftovers are swept on the next start of either
//! binary by [`cleanup_old_binaries`].
//!
//! ## Channels
//! GitHub excludes pre-releases from "latest", and so does this module unless
//! the user has opted into betas. A beta user still gets a stable release when
//! that is the newer one; opting back out never downgrades anyone.

use crate::{BkError, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The repository releases are published from.
pub const REPO: &str = "LittleGroove/BackupUtility";
/// The page a person opens to read release notes or download by hand.
pub const RELEASES_URL: &str = "https://github.com/LittleGroove/BackupUtility/releases";
/// The version compiled into this binary.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// The checksum file every release carries.
pub const CHECKSUMS: &str = "SHA256SUMS";
/// Scratch directory inside the install directory while an update downloads.
pub const UPDATE_DIR: &str = ".bku-update";

const USER_AGENT: &str = concat!("BackupUtility/", env!("CARGO_PKG_VERSION"));
const API_TIMEOUT: Duration = Duration::from_secs(8);
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Larger than any binary this project will ship; guards against a redirect
/// to something enormous.
const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Versions and platform naming
// ---------------------------------------------------------------------------

/// A `major.minor.patch` version. Orders the way people expect: 1.10.0 is
/// newer than 1.9.9.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// Parse `1.2.3`, `v1.2.3`, or `v1.2.3-beta` (the suffix is ignored, so a
    /// beta competes on the same counter as a stable release).
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim().trim_start_matches(['v', 'V']);
        let mut parts = s.splitn(3, '.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let rest = parts.next()?;
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        // Anything after the digits must be a `-suffix`, never more digits.
        match rest[digits.len()..].chars().next() {
            None | Some('-') | Some('+') => {}
            Some(_) => return None,
        }
        Some(Version {
            major,
            minor,
            patch: digits.parse().ok()?,
        })
    }

    pub fn current() -> Version {
        Version::parse(CURRENT_VERSION).expect("CARGO_PKG_VERSION is always well-formed")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// `windows-x86_64`, `linux-x86_64`, `linux-aarch64`, ... — the tag in every
/// platform-specific asset name. The release tooling uses the same spelling.
pub fn platform_tag() -> String {
    let os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        std::env::consts::OS
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

/// The two binaries that make up an install, by their short names.
pub const BINARIES: [&str; 2] = ["bkengine", "bkgui"];

/// The file name a binary has inside the install directory.
pub fn local_name(binary: &str) -> String {
    format!("{binary}{}", std::env::consts::EXE_SUFFIX)
}

/// The release-asset name of a binary for this platform.
pub fn asset_name(binary: &str) -> String {
    format!(
        "{binary}-{}{}",
        platform_tag(),
        std::env::consts::EXE_SUFFIX
    )
}

// ---------------------------------------------------------------------------
// Releases
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub prerelease: bool,
    /// The release notes, as written on GitHub (Markdown).
    pub notes: String,
    /// The release page.
    pub url: String,
    pub published: String,
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }

    /// Does this release carry what an update on this platform needs?
    pub fn has_platform_assets(&self) -> bool {
        self.asset(CHECKSUMS).is_some() && self.asset(&asset_name("bkengine")).is_some()
    }

    pub fn is_newer_than_current(&self) -> bool {
        self.version > Version::current()
    }
}

/// Parse the GitHub `releases` list. Drafts and releases whose tag is not a
/// version are dropped.
pub fn parse_releases(json: &str) -> Result<Vec<Release>> {
    let v: Value = serde_json::from_str(json)
        .map_err(|e| BkError::Other(anyhow::anyhow!("the releases list is not JSON: {e}")))?;
    let Some(items) = v.as_array() else {
        // GitHub answers an object with "message" for errors and rate limits.
        let msg = v
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unexpected response");
        return Err(BkError::Other(anyhow::anyhow!("GitHub said: {msg}")));
    };
    let s = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(items
        .iter()
        .filter(|r| !r.get("draft").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|r| {
            let tag = s(r, "tag_name");
            let version = Version::parse(&tag)?;
            let assets = r
                .get("assets")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|x| Asset {
                            name: s(x, "name"),
                            url: s(x, "browser_download_url"),
                            size: x.get("size").and_then(Value::as_u64).unwrap_or(0),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Release {
                version,
                tag,
                prerelease: r
                    .get("prerelease")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                notes: s(r, "body"),
                url: s(r, "html_url"),
                published: s(r, "published_at"),
                assets,
            })
        })
        .collect())
}

/// The highest-versioned release the user is eligible for. Pre-releases count
/// only with `beta`; on a tie between a stable and a beta of the same
/// version, the stable one wins.
pub fn pick(releases: &[Release], beta: bool) -> Option<&Release> {
    releases
        .iter()
        .filter(|r| beta || !r.prerelease)
        .max_by_key(|r| (r.version, !r.prerelease))
}

// ---------------------------------------------------------------------------
// Talking to GitHub
// ---------------------------------------------------------------------------

/// A client for the releases API. Tests point it at a local server.
pub struct Updater {
    api_base: String,
    api: ureq::Agent,
    download: ureq::Agent,
}

impl Default for Updater {
    fn default() -> Self {
        Self::github()
    }
}

impl Updater {
    pub fn github() -> Self {
        Self::with_api_base("https://api.github.com")
    }

    pub fn with_api_base(base: &str) -> Self {
        let api = ureq::Agent::config_builder()
            .timeout_global(Some(API_TIMEOUT))
            .user_agent(USER_AGENT)
            .build()
            .new_agent();
        let download = ureq::Agent::config_builder()
            .timeout_connect(Some(DOWNLOAD_CONNECT_TIMEOUT))
            .timeout_global(Some(DOWNLOAD_TIMEOUT))
            .user_agent(USER_AGENT)
            .build()
            .new_agent();
        Updater {
            api_base: base.trim_end_matches('/').to_string(),
            api,
            download,
        }
    }

    /// The newest 30 releases, newest first, pre-releases included.
    pub fn fetch_releases(&self) -> Result<Vec<Release>> {
        let url = format!("{}/repos/{REPO}/releases?per_page=30", self.api_base);
        let text = self.get_text(&self.api, &url)?;
        parse_releases(&text)
    }

    /// The release to offer, if any: newer than this binary, on the chosen
    /// channel, and carrying assets for this platform.
    pub fn check(&self, beta: bool) -> Result<Option<Release>> {
        let releases = self.fetch_releases()?;
        Ok(pick(&releases, beta)
            .filter(|r| r.is_newer_than_current() && r.has_platform_assets())
            .cloned())
    }

    fn get_text(&self, agent: &ureq::Agent, url: &str) -> Result<String> {
        let mut resp = agent
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call()
            .map_err(|e| describe(url, e))?;
        resp.body_mut()
            .read_to_string()
            .map_err(|e| describe(url, e))
    }

    /// Stream an asset to `dest`, reporting bytes so far and the total when
    /// known.
    pub fn download_to(
        &self,
        asset: &Asset,
        dest: &Path,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<()> {
        let mut resp = self
            .download
            .get(&asset.url)
            .call()
            .map_err(|e| describe(&asset.url, e))?;
        let total = resp.body_mut().content_length().or(if asset.size > 0 {
            Some(asset.size)
        } else {
            None
        });
        let mut reader = resp.body_mut().with_config().limit(MAX_DOWNLOAD).reader();
        let mut file = std::fs::File::create(dest)?;
        let mut buf = vec![0u8; 256 * 1024];
        let mut done = 0u64;
        progress(0, total);
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            done += n as u64;
            progress(done, total);
        }
        file.sync_all()?;
        Ok(())
    }
}

fn describe(url: &str, e: ureq::Error) -> BkError {
    let what = match &e {
        ureq::Error::StatusCode(403) => "GitHub refused the request (rate limit?)".to_string(),
        ureq::Error::StatusCode(404) => "not found".to_string(),
        ureq::Error::StatusCode(c) => format!("HTTP {c}"),
        ureq::Error::Timeout(_) => "timed out".to_string(),
        other => other.to_string(),
    };
    BkError::Other(anyhow::anyhow!("{what} ({url})"))
}

// ---------------------------------------------------------------------------
// Checksums
// ---------------------------------------------------------------------------

/// Parse `SHA256SUMS`: one `<hex>  <name>` (or `<hex> *<name>`) per line.
pub fn parse_checksums(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let (hash, name) = line.split_once(char::is_whitespace)?;
            let name = name.trim().trim_start_matches('*');
            // Names may be written with a path; only the file name matters.
            let name = name.rsplit(['/', '\\']).next()?;
            (hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
                .then(|| (name.to_string(), hash.to_ascii_lowercase()))
        })
        .collect()
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

// ---------------------------------------------------------------------------
// Applying an update
// ---------------------------------------------------------------------------

/// Progress of [`apply`], for a progress bar or a log line.
#[derive(Debug, Clone)]
pub enum UpdateEvent {
    Downloading {
        name: String,
        done: u64,
        total: Option<u64>,
    },
    Verifying(String),
    Installing(String),
}

/// What an update did.
#[derive(Debug, Clone)]
pub struct Applied {
    pub version: Version,
    /// The binaries now in place.
    pub replaced: Vec<PathBuf>,
    /// The path to start to run the new version of the updating executable,
    /// when it replaced itself.
    pub relaunch: Option<PathBuf>,
}

/// Download `release` and install it over the running executable's directory.
pub fn apply(
    updater: &Updater,
    release: &Release,
    progress: &mut dyn FnMut(UpdateEvent),
) -> Result<Applied> {
    let me = std::env::current_exe()?;
    let dir = me
        .parent()
        .ok_or_else(|| BkError::Other(anyhow::anyhow!("cannot find the install directory")))?
        .to_path_buf();
    apply_into(updater, release, &dir, Some(&me), progress)
}

/// The same with the target made explicit. `running` is the binary in `dir`
/// that is this process and must be replaced through `self-replace`; tests
/// pass `None` and a scratch directory.
pub fn apply_into(
    updater: &Updater,
    release: &Release,
    dir: &Path,
    running: Option<&Path>,
    progress: &mut dyn FnMut(UpdateEvent),
) -> Result<Applied> {
    let err = |m: String| BkError::Other(anyhow::anyhow!(m));

    // 1. The checksums come first; without them nothing is installed.
    let sums_asset = release.asset(CHECKSUMS).ok_or_else(|| {
        err(format!(
            "release {} has no {CHECKSUMS}; refusing to install unverified binaries",
            release.tag
        ))
    })?;
    let sums_path = {
        let scratch = dir.join(UPDATE_DIR);
        std::fs::create_dir_all(&scratch)?;
        scratch.join(CHECKSUMS)
    };
    updater.download_to(sums_asset, &sums_path, &mut |_, _| {})?;
    let sums = parse_checksums(&std::fs::read_to_string(&sums_path)?);

    // 2. Which binaries to update: the ones present here. A CLI-only install
    //    has no GUI and should not acquire one.
    let mut targets: Vec<(&str, PathBuf)> = BINARIES
        .iter()
        .map(|b| (*b, dir.join(local_name(b))))
        .filter(|(_, p)| p.exists())
        .collect();
    if targets.is_empty() {
        targets.push(("bkengine", dir.join(local_name("bkengine"))));
    }
    // Everything must be available before anything is swapped, so an update
    // is all or nothing.
    let mut plan = Vec::new();
    for (binary, target) in &targets {
        let name = asset_name(binary);
        let asset = release.asset(&name).ok_or_else(|| {
            err(format!(
                "release {} has no {name} for this platform",
                release.tag
            ))
        })?;
        let expected = sums
            .get(&name)
            .ok_or_else(|| {
                err(format!(
                    "{CHECKSUMS} in release {} does not list {name}",
                    release.tag
                ))
            })?
            .clone();
        plan.push((binary.to_string(), target.clone(), asset.clone(), expected));
    }

    // 3. Download and verify into the scratch directory.
    let scratch = dir.join(UPDATE_DIR);
    let mut staged = Vec::new();
    for (binary, target, asset, expected) in &plan {
        let tmp = scratch.join(&asset.name);
        updater.download_to(asset, &tmp, &mut |done, total| {
            progress(UpdateEvent::Downloading {
                name: asset.name.clone(),
                done,
                total,
            })
        })?;
        progress(UpdateEvent::Verifying(asset.name.clone()));
        let actual = sha256_file(&tmp)?;
        if actual != *expected {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(err(format!(
                "{} did not verify: expected sha256 {expected}, got {actual}. Nothing was installed.",
                asset.name
            )));
        }
        make_executable(&tmp)?;
        staged.push((binary.clone(), target.clone(), tmp));
    }

    // 4. Swap. The running executable goes last, so a failure there leaves the
    //    other binary already current rather than the other way round.
    let is_running = |target: &Path| running.is_some_and(|r| same_file(r, target));
    staged.sort_by_key(|(_, target, _)| is_running(target));
    let mut replaced = Vec::new();
    let mut relaunch = None;
    for (_, target, tmp) in &staged {
        progress(UpdateEvent::Installing(
            target
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        ));
        if is_running(target) {
            self_replace::self_replace(tmp).map_err(|e| {
                err(format!(
                    "could not replace the running executable {}: {e}",
                    target.display()
                ))
            })?;
            relaunch = Some(target.clone());
        } else {
            swap_file(tmp, target)
                .map_err(|e| err(format!("could not install {}: {e}", target.display())))?;
        }
        replaced.push(target.clone());
    }
    let _ = std::fs::remove_dir_all(&scratch);

    Ok(Applied {
        version: release.version,
        replaced,
        relaunch,
    })
}

/// Put `new` at `target`, moving any existing file (even one that is running)
/// aside to `<name>.old` first. Rolls the old file back if the final rename
/// fails, so `target` never ends up missing.
pub fn swap_file(new: &Path, target: &Path) -> std::io::Result<()> {
    let mut parked = None;
    if target.exists() {
        let aside = free_old_name(target);
        std::fs::rename(target, &aside)?;
        parked = Some(aside);
    }
    match std::fs::rename(new, target) {
        Ok(()) => Ok(()),
        Err(e) => {
            if let Some(aside) = parked {
                let _ = std::fs::rename(&aside, target);
            }
            Err(e)
        }
    }
}

/// `bkengine.exe.old`, or `.old-2`, `.old-3`, ... when an earlier leftover is
/// still locked by a process that has not exited yet.
fn free_old_name(target: &Path) -> PathBuf {
    let base = format!(
        "{}.old",
        target.file_name().unwrap_or_default().to_string_lossy()
    );
    let dir = target.parent().unwrap_or(Path::new("."));
    let first = dir.join(&base);
    if !first.exists() || std::fs::remove_file(&first).is_ok() {
        return first;
    }
    for n in 2..100 {
        let p = dir.join(format!("{base}-{n}"));
        if !p.exists() || std::fs::remove_file(&p).is_ok() {
            return p;
        }
    }
    first
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Delete what an earlier update left behind in `dir`: parked `*.old`
/// binaries, the scratch directory, and `self-replace`'s relocated copies.
/// Best-effort — a file still held by a process that has not exited is left
/// for next time. Returns how many entries were removed.
pub fn cleanup_old_binaries(dir: &Path) -> usize {
    let mut removed = 0;
    let scratch = dir.join(UPDATE_DIR);
    if scratch.exists() && std::fs::remove_dir_all(&scratch).is_ok() {
        removed += 1;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return removed;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ours = BINARIES.iter().any(|b| name.starts_with(&local_name(b)));
        let parked = ours && name.contains(".old");
        let relocated = name.contains(".__relocated__.")
            || name.contains(".__selfdelete__.")
            || name.contains(".__temp__.");
        if (parked || relocated) && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Start `exe` detached from this process, so this process can exit.
pub fn relaunch(exe: &Path, args: &[String]) -> std::io::Result<u32> {
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    Ok(cmd.spawn()?.id())
}

/// Short, human description of a check result for a log line or a toast.
pub fn describe_check(result: &Result<Option<Release>>) -> String {
    match result {
        Ok(Some(r)) => format!(
            "version {} is available (you have {})",
            r.version, CURRENT_VERSION
        ),
        Ok(None) => format!("you have the newest version ({CURRENT_VERSION})"),
        Err(e) => format!("could not check for updates: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;

    // -- versions -----------------------------------------------------------

    #[test]
    fn versions_parse_and_order_numerically() {
        let v = |s| Version::parse(s).unwrap();
        assert_eq!(
            v("1.2.3"),
            Version {
                major: 1,
                minor: 2,
                patch: 3
            }
        );
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert_eq!(
            v("v1.2.3-beta"),
            v("1.2.3"),
            "a beta competes on the bare number"
        );
        assert_eq!(v("1.2.3+build7"), v("1.2.3"));
        assert!(v("1.10.0") > v("1.9.9"));
        assert!(v("2.0.0") > v("1.99.99"));
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.x").is_none());
        assert!(Version::parse("1.2.3x").is_none());
        assert!(Version::parse("latest").is_none());
        assert_eq!(v("3.4.5").to_string(), "3.4.5");
        assert_eq!(Version::current().to_string(), CURRENT_VERSION);
    }

    #[test]
    fn asset_names_follow_the_platform() {
        let tag = platform_tag();
        assert!(tag.contains('-'), "{tag}");
        assert_eq!(
            asset_name("bkengine"),
            format!("bkengine-{tag}{}", std::env::consts::EXE_SUFFIX)
        );
        assert_eq!(
            local_name("bkgui"),
            format!("bkgui{}", std::env::consts::EXE_SUFFIX)
        );
    }

    // -- releases -----------------------------------------------------------

    fn release_json(entries: &[(&str, bool, bool)]) -> String {
        // (tag, prerelease, draft)
        let items: Vec<String> = entries
            .iter()
            .map(|(tag, pre, draft)| {
                format!(
                    r#"{{"tag_name":"{tag}","prerelease":{pre},"draft":{draft},"body":"notes for {tag}",
                        "html_url":"https://example/{tag}","published_at":"2026-10-09T00:00:00Z",
                        "assets":[{{"name":"SHA256SUMS","browser_download_url":"https://example/{tag}/SHA256SUMS","size":10}},
                                  {{"name":"{engine}","browser_download_url":"https://example/{tag}/{engine}","size":123}}]}}"#,
                    engine = asset_name("bkengine")
                )
            })
            .collect();
        format!("[{}]", items.join(","))
    }

    #[test]
    fn releases_parse_and_drafts_are_dropped() {
        let json = release_json(&[
            ("v1.2.0", false, false),
            ("v1.3.0", false, true),
            ("not-a-version", false, false),
        ]);
        let rels = parse_releases(&json).unwrap();
        assert_eq!(rels.len(), 1, "the draft and the nonsense tag are skipped");
        assert_eq!(rels[0].version, Version::parse("1.2.0").unwrap());
        assert_eq!(rels[0].notes, "notes for v1.2.0");
        assert!(rels[0].has_platform_assets());
        assert_eq!(rels[0].asset(CHECKSUMS).unwrap().size, 10);
    }

    #[test]
    fn an_api_error_object_is_reported_not_misread_as_no_releases() {
        let err = parse_releases(r#"{"message":"API rate limit exceeded"}"#).unwrap_err();
        assert!(err.to_string().contains("rate limit"), "{err}");
    }

    #[test]
    fn picking_honours_the_channel_and_prefers_stable_on_a_tie() {
        let json = release_json(&[
            ("v1.2.0", false, false),
            ("v1.3.0-beta", true, false),
            ("v1.1.0", false, false),
            ("v1.2.0-beta", true, false),
        ]);
        let rels = parse_releases(&json).unwrap();
        let stable = pick(&rels, false).unwrap();
        assert_eq!(
            stable.tag, "v1.2.0",
            "betas are invisible without opting in"
        );
        let beta = pick(&rels, true).unwrap();
        assert_eq!(
            beta.tag, "v1.3.0-beta",
            "a beta user gets the newest of either channel"
        );

        let tie = parse_releases(&release_json(&[
            ("v2.0.0-beta", true, false),
            ("v2.0.0", false, false),
        ]))
        .unwrap();
        assert_eq!(pick(&tie, true).unwrap().tag, "v2.0.0");
        assert!(pick(&[], true).is_none());
    }

    // -- checksums ----------------------------------------------------------

    #[test]
    fn checksum_files_parse_in_both_common_layouts() {
        let h = "a".repeat(64);
        let text = format!("{h}  bkengine-linux-x86_64\n{h} *bkgui-windows-x86_64.exe\n{h}  release/SHA256SUMS\nnot a line\n{}  short.bin\n", "b".repeat(10));
        let sums = parse_checksums(&text);
        assert_eq!(sums.get("bkengine-linux-x86_64").unwrap(), &h);
        assert_eq!(sums.get("bkgui-windows-x86_64.exe").unwrap(), &h);
        assert_eq!(
            sums.get("SHA256SUMS").unwrap(),
            &h,
            "paths are reduced to the file name"
        );
        assert!(
            !sums.contains_key("short.bin"),
            "a malformed hash is ignored"
        );
        assert_eq!(sums.len(), 3);
    }

    #[test]
    fn sha256_of_a_file_matches_the_known_digest() {
        let p = std::env::temp_dir().join("bku_update_sha_test.bin");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&p);
    }

    // -- swapping -----------------------------------------------------------

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(n: &str) -> Self {
            let p = std::env::temp_dir().join(format!("bku_update_{n}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn swapping_parks_the_old_file_and_rolls_back_on_failure() {
        let t = Tmp::new("swap");
        let target = t.0.join(local_name("bkengine"));
        let new = t.0.join("new.bin");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        swap_file(&new, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(
            std::fs::read(t.0.join(format!("{}.old", local_name("bkengine")))).unwrap(),
            b"old"
        );
        assert!(!new.exists());

        // A second update parks another copy without clobbering a locked one.
        let new2 = t.0.join("new2.bin");
        std::fs::write(&new2, b"newer").unwrap();
        swap_file(&new2, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"newer");

        // The new file vanishing before the rename must leave the old one in place.
        let missing = t.0.join("does-not-exist.bin");
        assert!(swap_file(&missing, &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"newer", "rolled back");

        // Leftovers of every kind are swept; the live binary is not.
        std::fs::write(t.0.join(format!("{}.old", local_name("bkengine"))), b"x").unwrap();
        std::fs::write(t.0.join(format!("{}.old-2", local_name("bkgui"))), b"x").unwrap();
        std::fs::write(t.0.join("bkgui.__relocated__.exe"), b"x").unwrap();
        std::fs::create_dir_all(t.0.join(UPDATE_DIR)).unwrap();
        assert_eq!(cleanup_old_binaries(&t.0), 4);
        assert!(!t.0.join(format!("{}.old", local_name("bkengine"))).exists());
        assert!(target.exists(), "cleanup never touches the live binary");
    }

    // -- end to end against a local server ----------------------------------

    /// A one-thread HTTP/1.1 server answering GETs from a fixed route table.
    fn serve(routes: Vec<(String, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h == "\r\n" || h == "\n" || h.is_empty()
                    {
                        break;
                    }
                }
                let mut stream = stream;
                match routes.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => {
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n", body.len());
                        let _ = stream.write_all(body);
                    }
                    None => {
                        let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
            }
        });
        format!("http://{addr}")
    }

    fn fake_release(
        base: &str,
        engine: &[u8],
        gui: &[u8],
        sums_for_gui: bool,
    ) -> (Vec<(String, Vec<u8>)>, String) {
        let engine_name = asset_name("bkengine");
        let gui_name = asset_name("bkgui");
        let sha = |b: &[u8]| format!("{:x}", Sha256::digest(b));
        let mut sums = format!("{}  {engine_name}\n", sha(engine));
        if sums_for_gui {
            sums.push_str(&format!("{}  {gui_name}\n", sha(gui)));
        }
        let asset = |n: &str, size: usize| {
            format!(r#"{{"name":"{n}","browser_download_url":"{base}/dl/{n}","size":{size}}}"#)
        };
        let json = format!(
            r#"[{{"tag_name":"v99.0.0","prerelease":false,"draft":false,"body":"big","html_url":"{base}/rel","published_at":"","assets":[{},{},{}]}}]"#,
            asset(CHECKSUMS, sums.len()),
            asset(&engine_name, engine.len()),
            asset(&gui_name, gui.len())
        );
        let routes = vec![
            (
                format!("/repos/{REPO}/releases?per_page=30"),
                json.into_bytes(),
            ),
            (format!("/dl/{CHECKSUMS}"), sums.into_bytes()),
            (format!("/dl/{engine_name}"), engine.to_vec()),
            (format!("/dl/{gui_name}"), gui.to_vec()),
        ];
        (routes, engine_name)
    }

    #[test]
    fn an_update_downloads_verifies_and_installs_both_binaries() {
        // Two passes: the route table needs the base URL, which the server
        // hands out only once bound. Bind, then build routes that point back.
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let base = format!("http://127.0.0.1:{port}");
        let (routes, _) = fake_release(&base, b"ENGINE v99", b"GUI v99", true);
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve_one(stream, &routes);
            }
        });

        let t = Tmp::new("apply");
        std::fs::write(t.0.join(local_name("bkengine")), b"old engine").unwrap();
        std::fs::write(t.0.join(local_name("bkgui")), b"old gui").unwrap();

        let up = Updater::with_api_base(&base);
        let rel = up
            .check(false)
            .unwrap()
            .expect("v99 is newer than anything we build");
        assert_eq!(rel.version.to_string(), "99.0.0");

        let mut events = Vec::new();
        let applied = apply_into(&up, &rel, &t.0, None, &mut |e| {
            events.push(format!("{e:?}"))
        })
        .unwrap();
        assert_eq!(applied.replaced.len(), 2);
        assert!(applied.relaunch.is_none(), "nothing was running");
        assert_eq!(
            std::fs::read(t.0.join(local_name("bkengine"))).unwrap(),
            b"ENGINE v99"
        );
        assert_eq!(
            std::fs::read(t.0.join(local_name("bkgui"))).unwrap(),
            b"GUI v99"
        );
        assert!(!t.0.join(UPDATE_DIR).exists(), "scratch is removed");
        assert!(events.iter().any(|e| e.starts_with("Downloading")));
        assert!(events.iter().any(|e| e.starts_with("Verifying")));
        assert!(events.iter().any(|e| e.starts_with("Installing")));
        // The old files are parked, then swept.
        assert!(t.0.join(format!("{}.old", local_name("bkgui"))).exists());
        assert_eq!(cleanup_old_binaries(&t.0), 2);
    }

    #[test]
    fn a_bad_checksum_installs_nothing() {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let base = format!("http://127.0.0.1:{port}");
        let (mut routes, engine_name) = fake_release(&base, b"ENGINE v99", b"GUI v99", true);
        // Serve different bytes than the checksum file promises.
        for (p, body) in routes.iter_mut() {
            if *p == format!("/dl/{engine_name}") {
                *body = b"TAMPERED".to_vec();
            }
        }
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve_one(stream, &routes);
            }
        });

        let t = Tmp::new("badsum");
        std::fs::write(t.0.join(local_name("bkengine")), b"old engine").unwrap();
        let up = Updater::with_api_base(&base);
        let rel = up.check(false).unwrap().unwrap();
        let err = apply_into(&up, &rel, &t.0, None, &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not verify"), "{err}");
        assert_eq!(
            std::fs::read(t.0.join(local_name("bkengine"))).unwrap(),
            b"old engine"
        );
        assert!(!t.0.join(UPDATE_DIR).exists());
    }

    #[test]
    fn a_release_missing_a_checksum_entry_is_refused_before_anything_moves() {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let base = format!("http://127.0.0.1:{port}");
        let (routes, _) = fake_release(&base, b"ENGINE v99", b"GUI v99", false);
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve_one(stream, &routes);
            }
        });

        let t = Tmp::new("nosum");
        std::fs::write(t.0.join(local_name("bkengine")), b"old engine").unwrap();
        std::fs::write(t.0.join(local_name("bkgui")), b"old gui").unwrap();
        let up = Updater::with_api_base(&base);
        let rel = up.check(false).unwrap().unwrap();
        let err = apply_into(&up, &rel, &t.0, None, &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not list"), "{err}");
        assert_eq!(
            std::fs::read(t.0.join(local_name("bkengine"))).unwrap(),
            b"old engine"
        );
        assert_eq!(
            std::fs::read(t.0.join(local_name("bkgui"))).unwrap(),
            b"old gui"
        );
    }

    fn serve_one(stream: std::net::TcpStream, routes: &[(String, Vec<u8>)]) {
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).is_err() || h == "\r\n" || h == "\n" || h.is_empty() {
                break;
            }
        }
        let mut stream = stream;
        match routes.iter().find(|(p, _)| *p == path) {
            Some((_, body)) => {
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(body);
            }
            None => {
                let _ = write!(
                    stream,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        }
    }

    #[test]
    fn the_local_server_helper_answers_404_for_unknown_paths() {
        let base = serve(vec![("/ok".into(), b"fine".to_vec())]);
        let up = Updater::with_api_base(&base);
        assert_eq!(up.get_text(&up.api, &format!("{base}/ok")).unwrap(), "fine");
        let err = up
            .get_text(&up.api, &format!("{base}/missing"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
    }
}

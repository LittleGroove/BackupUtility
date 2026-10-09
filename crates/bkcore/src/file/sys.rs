//! The platform seam.
//!
//! Every OS-specific fact the file engine needs is asked for through this
//! module, so `walk`, `plan`, `copy` and `mirror` stay portable. The Windows
//! implementation is the complete one; the Unix implementation is real where
//! Rust's standard library already covers the ground (link targets, identity,
//! permissions) and honestly reports "unsupported" where it does not, rather
//! than silently pretending a backup preserved something it did not.

use crate::{BkError, Result};
use std::path::Path;

use super::meta::FileId;

// ===========================================================================
// Windows
// ===========================================================================
#[cfg(windows)]
mod imp {
    use super::*;
    use crate::file::path::long;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, MAX_PATH};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FindClose, FindFirstFileW, GetFileInformationByHandle, GetVolumeInformationW,
        GetVolumePathNameW, BY_HANDLE_FILE_INFORMATION, FILE_FLAGS_AND_ATTRIBUTES,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, WIN32_FIND_DATAW,
    };

    /// A `HANDLE` that closes itself. Every early return in this module would
    /// otherwise be a handle leak, and a backup process that runs for hours
    /// across millions of files cannot afford even a slow leak.
    pub struct OwnedHandle(pub HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: the handle came from CreateFileW and is closed once.
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    pub fn wide(p: &Path) -> Vec<u16> {
        long(p)
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Open a handle purely to interrogate metadata.
    ///
    /// `BACKUP_SEMANTICS` is what lets us open a *directory* handle at all, and
    /// `OPEN_REPARSE_POINT` makes us open the link itself rather than following
    /// it — without that, asking about a junction would answer about its target.
    /// We request no access rights (`0`), which is enough for metadata queries
    /// and succeeds on files whose ACLs would deny a read.
    fn open_for_meta(path: &Path) -> Result<OwnedHandle> {
        let w = wide(path);
        // SAFETY: `w` is a valid null-terminated UTF-16 string outliving the call.
        let h = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                HANDLE::default(),
            )
        }
        .map_err(|e| BkError::Other(anyhow::anyhow!("open {}: {e}", path.display())))?;
        Ok(OwnedHandle(h))
    }

    /// The reparse tag on `path`, or 0 if it is not a reparse point.
    ///
    /// Read from the directory-entry data rather than by opening the file,
    /// because the scanner calls this for every reparse point it meets and an
    /// open is far more expensive than a find.
    pub fn reparse_tag(path: &Path) -> u32 {
        let w = wide(path);
        let mut data = WIN32_FIND_DATAW::default();
        // SAFETY: valid null-terminated path; `data` is a valid out-param.
        let h = match unsafe { FindFirstFileW(PCWSTR(w.as_ptr()), &mut data) } {
            Ok(h) if !h.is_invalid() => h,
            _ => return 0,
        };
        // SAFETY: handle came from FindFirstFileW and is closed exactly once.
        unsafe {
            let _ = FindClose(h);
        }
        // dwReserved0 holds the reparse tag, but ONLY when the attribute bit
        // is set; otherwise it is undefined garbage.
        if data.dwFileAttributes & super::super::meta::attr::REPARSE_POINT != 0 {
            data.dwReserved0
        } else {
            0
        }
    }

    /// The file's on-volume identity plus its hard-link count.
    pub fn file_identity(path: &Path) -> Result<(FileId, u64)> {
        let h = open_for_meta(path)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: valid handle; `info` is a valid out-param of the right type.
        unsafe { GetFileInformationByHandle(h.0, &mut info) }
            .map_err(|e| BkError::Other(anyhow::anyhow!("file id {}: {e}", path.display())))?;
        let index = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
        Ok((
            FileId {
                volume: info.dwVolumeSerialNumber as u64,
                index,
            },
            info.nNumberOfLinks as u64,
        ))
    }

    /// The filesystem name of the volume containing `path`, e.g. `"NTFS"`.
    ///
    /// Used to pick a timestamp comparison tolerance: a FAT destination cannot
    /// store an NTFS timestamp exactly, and comparing exactly would re-copy
    /// every file on every run.
    pub fn filesystem_name(path: &Path) -> Option<String> {
        let w = wide(path);
        let mut root = vec![0u16; MAX_PATH as usize + 8];
        // SAFETY: both buffers are valid and correctly sized.
        unsafe { GetVolumePathNameW(PCWSTR(w.as_ptr()), &mut root) }.ok()?;

        let mut fs_name = vec![0u16; 64];
        // SAFETY: `root` is null-terminated by the call above; out buffers valid.
        unsafe {
            GetVolumeInformationW(
                PCWSTR(root.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut fs_name),
            )
        }
        .ok()?;
        let end = fs_name
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(fs_name.len());
        Some(String::from_utf16_lossy(&fs_name[..end]))
    }

    /// Bytes available to us on the volume holding `path`.
    ///
    /// `GetDiskFreeSpaceExW` rather than the NTFS volume-data IOCTL, because
    /// this one needs no elevation and reports the caller's *quota-adjusted*
    /// free space — which is the number that decides whether the copy will
    /// actually fit.
    pub fn free_space(path: &Path) -> Option<u64> {
        let w = wide(path);
        let mut available: u64 = 0;
        // SAFETY: valid null-terminated path; `available` is a valid out-param
        // and the two we do not need are passed as None.
        unsafe {
            windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                PCWSTR(w.as_ptr()),
                Some(&mut available),
                None,
                None,
            )
        }
        .ok()?;
        Some(available)
    }

    /// True if the current process holds Administrator rights.
    pub fn is_elevated() -> bool {
        crate::vss::is_elevated()
    }

    pub const SUPPORTS_ACLS: bool = true;
    pub const SUPPORTS_ADS: bool = true;
    pub const SUPPORTS_SPARSE: bool = true;

    pub fn _unused(_: FILE_FLAGS_AND_ATTRIBUTES) {}
}

// ===========================================================================
// Unix
// ===========================================================================
#[cfg(not(windows))]
mod imp {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// Unix has no reparse points. Symlinks are reported by the scanner via
    /// `file_type().is_symlink()`, so there is no tag to fetch.
    pub fn reparse_tag(_path: &Path) -> u32 {
        0
    }

    /// `(device, inode)` is the exact Unix analogue of Windows' volume+index,
    /// and `st_nlink` is the hard-link count — so hard-link preservation works
    /// identically on both platforms.
    pub fn file_identity(path: &Path) -> Result<(FileId, u64)> {
        let md = std::fs::symlink_metadata(path)?;
        Ok((
            FileId {
                volume: md.dev(),
                index: md.ino(),
            },
            md.nlink(),
        ))
    }

    /// The filesystem type holding `path`, from `/proc/self/mounts`: `ext4`,
    /// `ntfs3`, `vfat`, `exfat`, ... The path need not exist yet (a
    /// destination folder usually does not); the nearest existing ancestor
    /// decides.
    pub fn filesystem_name(path: &Path) -> Option<String> {
        let text = std::fs::read_to_string("/proc/self/mounts").ok()?;
        let mounts = crate::mounts::parse(&text);
        let probe = nearest_existing(path)?;
        let canon = std::fs::canonicalize(&probe).unwrap_or(probe);
        crate::mounts::find(&mounts, &canon.to_string_lossy()).map(|m| m.fstype.clone())
    }

    /// Bytes available on the filesystem holding `path`, via `statvfs`.
    ///
    /// `f_bavail` rather than `f_bfree`: the difference is the root-reserved
    /// blocks, which an ordinary backup process cannot use.
    pub fn free_space(path: &Path) -> Option<u64> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: `c` is a valid null-terminated path and `st` is a live
        // out-param of exactly the type statvfs expects.
        unsafe {
            let mut st: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c.as_ptr(), &mut st) != 0 {
                return None;
            }
            Some(st.f_bavail as u64 * st.f_frsize as u64)
        }
    }

    pub fn is_elevated() -> bool {
        // SAFETY: `geteuid` is a pure read of process state and cannot fail.
        unsafe { libc_geteuid() == 0 }
    }

    // Declared directly to avoid pulling in the `libc` crate for one call.
    extern "C" {
        #[link_name = "geteuid"]
        fn libc_geteuid() -> u32;
    }

    /// POSIX ACLs need `libacl`, which we do not link. Mode bits are preserved
    /// instead; see `copy::apply_permissions`.
    pub const SUPPORTS_ACLS: bool = false;
    /// Extended attributes are the nearest analogue to NTFS alternate data
    /// streams, but they are not the same thing and are not mirrored yet.
    pub const SUPPORTS_ADS: bool = false;
    /// `SEEK_HOLE`/`SEEK_DATA` sparse detection is implemented in `copy`.
    pub const SUPPORTS_SPARSE: bool = true;
}

pub use imp::{
    file_identity, filesystem_name, free_space, is_elevated, reparse_tag, SUPPORTS_ACLS,
    SUPPORTS_ADS, SUPPORTS_SPARSE,
};

#[cfg(windows)]
pub use imp::{wide, OwnedHandle};

/// Let a pipe closing early (`bkengine status | head`) end the process
/// quietly instead of panicking on the next `println!`. Rust ignores SIGPIPE
/// by default so writes report EPIPE; a command-line tool wants the shell
/// convention back. No-op on Windows.
pub fn reset_sigpipe() {
    #[cfg(unix)]
    {
        // SAFETY: restoring the default disposition of a signal is always valid.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
}

/// `path` itself if it exists, else its closest existing ancestor. A
/// destination folder usually does not exist before the first run, but the
/// volume it will live on does.
pub fn nearest_existing(path: &Path) -> Option<std::path::PathBuf> {
    let mut p = path.to_path_buf();
    loop {
        if p.exists() {
            return Some(p);
        }
        if !p.pop() {
            return None;
        }
    }
}

/// Pick a timestamp comparison tolerance from the destination's filesystem.
///
/// Called once per run, not per file.
pub fn time_tolerance_for(path: &Path) -> u64 {
    tolerance_for_filesystem(filesystem_name(path).as_deref())
}

/// The filesystem-to-tolerance mapping, split out from the I/O so it can be
/// tested without needing a volume of each kind to hand.
///
/// Different filesystems store timestamps at different resolutions, and the
/// destination is the one that has to hold what we write. Comparing exactly
/// against a filesystem that cannot represent the source's precision would
/// re-copy the entire tree on every run.
pub fn tolerance_for_filesystem(name: Option<&str>) -> u64 {
    use super::meta::*;
    let Some(name) = name else {
        // Unknown or network filesystem: be forgiving. Re-copying a file we
        // did not need to is wasteful; re-copying the whole tree every night
        // because of a 1 ms rounding difference is unusable.
        return TOLERANCE_UNKNOWN;
    };
    // Windows reports `NTFS`; Linux reports `ntfs3` (kernel) or `fuseblk`
    // (ntfs-3g, which is also any other FUSE filesystem, so it stays unknown).
    match name.to_ascii_lowercase().as_str() {
        "ntfs" | "ntfs3" | "refs" | "ext4" | "xfs" | "btrfs" | "f2fs" | "zfs" | "bcachefs"
        | "tmpfs" | "apfs" => TOLERANCE_NTFS,
        "exfat" => TOLERANCE_EXFAT,
        "ext2" | "ext3" | "hfsplus" | "jfs" | "reiserfs" => TOLERANCE_SECOND,
        fs if fs.starts_with("fat") || fs == "vfat" || fs == "msdos" => TOLERANCE_FAT,
        _ => TOLERANCE_UNKNOWN,
    }
}

/// Ensure the process can do the job asked of it, with an actionable message.
pub fn require_elevation(what: &str) -> Result<()> {
    if is_elevated() {
        Ok(())
    } else {
        Err(BkError::Other(anyhow::anyhow!(
            "{what} requires Administrator — re-run from an elevated prompt"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tolerance_is_exact_on_ntfs_and_forgiving_elsewhere() {
        use super::super::meta::*;
        assert_eq!(tolerance_for_filesystem(Some("NTFS")), TOLERANCE_NTFS);
        assert_eq!(tolerance_for_filesystem(Some("ReFS")), TOLERANCE_NTFS);
        assert_eq!(tolerance_for_filesystem(Some("exFAT")), TOLERANCE_EXFAT);
        assert_eq!(tolerance_for_filesystem(Some("FAT32")), TOLERANCE_FAT);
        assert_eq!(tolerance_for_filesystem(Some("FAT")), TOLERANCE_FAT);

        // The important case: something we did not recognize must NOT get the
        // exact-match tolerance, or a network share whose timestamps round
        // slightly would re-copy the whole tree every night.
        assert_eq!(tolerance_for_filesystem(None), TOLERANCE_UNKNOWN);
        assert_eq!(tolerance_for_filesystem(Some("NFS")), TOLERANCE_UNKNOWN);
        assert!(tolerance_for_filesystem(Some("something new")) > TOLERANCE_NTFS);
    }

    #[cfg(windows)]
    #[test]
    fn a_real_local_volume_resolves_to_a_usable_tolerance() {
        use super::super::meta::TOLERANCE_UNKNOWN;
        // Whatever the temp volume is, we must get a defined answer for it
        // rather than panicking or hanging.
        let t = time_tolerance_for(&std::env::temp_dir());
        assert!(t <= TOLERANCE_UNKNOWN);
    }

    #[cfg(windows)]
    #[test]
    fn an_ordinary_file_has_no_reparse_tag() {
        let dir = std::env::temp_dir().join("bku_sys_tag_test");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("plain.txt");
        std::fs::write(&f, b"hi").unwrap();
        assert_eq!(reparse_tag(&f), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn file_identity_pairs_hard_links_and_separates_distinct_files() {
        let dir = std::env::temp_dir().join("bku_sys_id_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, b"same").unwrap();
        std::fs::write(&b, b"same").unwrap();

        let (id_a, _) = file_identity(&a).unwrap();
        let (id_b, _) = file_identity(&b).unwrap();
        // Identical *contents* must not look like the same file.
        assert_ne!(id_a, id_b);

        // A real hard link must.
        let link = dir.join("a_link.txt");
        if std::fs::hard_link(&a, &link).is_ok() {
            let (id_link, nlink) = file_identity(&link).unwrap();
            assert_eq!(id_a, id_link);
            assert!(nlink >= 2, "hard link count should be at least 2");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn free_space_is_reported_for_a_real_volume() {
        // The preflight check depends on this, and a check that silently
        // answers None never fires.
        let free = free_space(&std::env::temp_dir());
        assert!(
            free.is_some(),
            "expected a free-space figure for the temp volume"
        );
        assert!(
            free.unwrap() > 0,
            "a writable volume should have some space"
        );
    }

    #[test]
    fn free_space_on_a_nonexistent_path_is_none_not_zero() {
        // Zero would read as "the disk is full" and block every job.
        let missing = std::env::temp_dir()
            .join("bku_no_such_volume_xyz")
            .join("deeper");
        let _ = std::fs::remove_dir_all(&missing);
        assert_eq!(free_space(&missing), None);
    }

    #[cfg(windows)]
    #[test]
    fn filesystem_name_resolves_for_a_real_path() {
        let tmp = std::env::temp_dir();
        // Whatever it is, we should get *something* back for a real local path.
        let fs = filesystem_name(&tmp);
        assert!(fs.is_some(), "expected a filesystem name for {tmp:?}");
    }
}

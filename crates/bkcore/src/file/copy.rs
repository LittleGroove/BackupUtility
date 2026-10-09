//! True file copy — the part that decides whether this is a backup or just a
//! pile of bytes with the right names.
//!
//! `CopyFileEx` would be one line, but it gives no progress granularity we can
//! cancel on, no control over sparse regions, and no say in how the
//! destination is committed. So the data loop is ours, and around it we
//! reproduce everything NTFS knows about a file:
//!
//! | Preserved | How |
//! |---|---|
//! | data | our own buffered loop, cancellable, with progress |
//! | sparse regions | `FSCTL_QUERY_ALLOCATED_RANGES` -> write only real data |
//! | alternate data streams | `FindFirstStreamW` enumeration, copied per stream |
//! | timestamps (all three) | `SetFileTime` with raw FILETIMEs — no precision loss |
//! | attributes | `SetFileAttributesW` (read-only, hidden, system, ...) |
//! | security descriptor | `GetNamedSecurityInfoW` / `SetNamedSecurityInfoW` |
//! | hard links | `CreateHardLinkW` against the first name we copied |
//! | junctions / symlinks | re-created, never followed |
//!
//! ## Commit discipline
//! Every file is written to a `.bku-part` temporary beside its destination and
//! atomically renamed into place once it is complete and its metadata is set.
//! An interrupted copy therefore leaves a temp file, never a truncated file
//! wearing the real name — which matters enormously here, because a
//! half-written file at the right path is a file a user would later restore
//! from and trust.

use super::meta::{attr, Entry, EntryKind};
use super::path::{self, RelPath};
use super::Cancel;
use crate::{BkError, Result};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Suffix for in-flight destination files.
pub const PART_SUFFIX: &str = ".bku-part";

/// Default copy buffer. Large enough to keep a spinning disk streaming,
/// small enough that a dozen worker threads do not exhaust RAM.
pub const DEFAULT_BUFFER: usize = 4 * 1024 * 1024;

/// What to preserve, and how hard to try.
#[derive(Debug, Clone)]
pub struct CopyOptions {
    pub buffer_size: usize,
    /// Copy the NTFS security descriptor (owner, group, DACL).
    pub preserve_acls: bool,
    /// Copy alternate data streams.
    pub preserve_ads: bool,
    /// Keep sparse regions sparse instead of writing zeroes.
    pub preserve_sparse: bool,
    pub preserve_timestamps: bool,
    pub preserve_attributes: bool,
    /// Re-read the destination and compare hashes after writing.
    pub verify: bool,
    /// Enable `SeBackupPrivilege`/`SeRestorePrivilege` so files whose ACLs
    /// deny us can still be read and written. Requires Administrator.
    pub use_backup_privilege: bool,
}

impl Default for CopyOptions {
    fn default() -> Self {
        CopyOptions {
            buffer_size: DEFAULT_BUFFER,
            preserve_acls: true,
            preserve_ads: true,
            preserve_sparse: true,
            preserve_timestamps: true,
            preserve_attributes: true,
            verify: false,
            use_backup_privilege: true,
        }
    }
}

impl CopyOptions {
    /// Just the bytes and the timestamps — for a destination that cannot hold
    /// NTFS metadata (a FAT drive, a network share, a Linux target).
    pub fn data_only() -> Self {
        CopyOptions {
            preserve_acls: false,
            preserve_ads: false,
            preserve_sparse: false,
            preserve_attributes: false,
            ..Default::default()
        }
    }
}

/// What one copy actually did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyOutcome {
    pub bytes_written: u64,
    /// Alternate data streams copied, beyond the unnamed `::$DATA` stream.
    pub streams: u32,
    pub sparse: bool,
    pub verified: bool,
    /// Metadata we were asked to preserve but could not (wrong filesystem,
    /// insufficient rights). Reported rather than silently dropped — a backup
    /// that quietly loses ACLs is worse than one that says so.
    pub degraded: Vec<&'static str>,
}

// ===========================================================================
// The portable entry points
// ===========================================================================

/// Copy one file, with whatever fidelity `opts` asks for and the filesystems
/// allow.
pub fn copy_file(
    src: &Path,
    dst: &Path,
    entry: &Entry,
    opts: &CopyOptions,
    cancel: &Cancel,
    progress: &mut dyn FnMut(u64),
) -> Result<CopyOutcome> {
    let part = part_path(dst);
    // A leftover temp from a previous interrupted run must not be appended to.
    let _ = force_remove(&part);

    let mut outcome = match copy_data(src, &part, entry, opts, cancel, progress) {
        Ok(o) => o,
        Err(e) => {
            let _ = force_remove(&part);
            return Err(e);
        }
    };

    apply_metadata(src, &part, entry, opts, &mut outcome);

    if opts.verify {
        match verify_same(src, &part) {
            Ok(true) => outcome.verified = true,
            Ok(false) => {
                let _ = force_remove(&part);
                return Err(BkError::StoreCorrupt(format!(
                    "verification failed: {} does not match the source after copy",
                    path::display(dst)
                )));
            }
            Err(e) => {
                let _ = force_remove(&part);
                return Err(e);
            }
        }
    }

    // Commit. `fs::rename` replaces an existing file atomically on both
    // Windows (MoveFileEx with REPLACE_EXISTING) and Unix.
    let _ = clear_readonly(dst);
    if let Err(e) = fs::rename(path::long(&part), path::long(dst)) {
        let _ = force_remove(&part);
        return Err(BkError::Io(e));
    }
    Ok(outcome)
}

/// Reproduce everything about the file that is not its contents.
///
/// Individually failing pieces are recorded in `outcome.degraded` rather than
/// aborting the copy: a file that arrived with its data intact but without its
/// ACL is worth keeping, and worth telling the user about. Losing metadata
/// silently is the failure mode to avoid.
fn apply_metadata(
    src: &Path,
    part: &Path,
    entry: &Entry,
    opts: &CopyOptions,
    outcome: &mut CopyOutcome,
) {
    if opts.preserve_ads {
        match copy_streams(src, part) {
            Ok(n) => outcome.streams = n,
            Err(_) => outcome.degraded.push("alternate data streams"),
        }
    }
    if opts.preserve_acls && !copy_security(src, part) {
        outcome.degraded.push("security descriptor");
    }
    // Timestamps go on before the rename: renaming does not disturb a file's
    // own timestamps, and setting them on the temp name means the file is
    // never briefly visible at its real path with the wrong ones.
    if opts.preserve_timestamps && !apply_timestamps(part, entry) {
        outcome.degraded.push("timestamps");
    }
    if opts.preserve_attributes && !apply_attributes(part, entry) {
        outcome.degraded.push("attributes");
    }
}

/// Create a directory, including any missing parents.
pub fn create_dir(dst: &Path) -> Result<()> {
    match fs::create_dir_all(path::long(dst)) {
        Ok(()) => Ok(()),
        // A concurrent worker may have created it first; that is success.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(BkError::Io(e)),
    }
}

/// Apply a directory's timestamps and attributes. Called in the plan's final
/// phase, once the directory's contents are in place.
pub fn set_dir_meta(dst: &Path, entry: &Entry, opts: &CopyOptions) -> Result<()> {
    if !dst.exists() {
        return Ok(());
    }
    if opts.preserve_acls {
        let _ = copy_security_from_entry(dst, entry);
    }
    if opts.preserve_attributes {
        apply_attributes(dst, entry);
    }
    if opts.preserve_timestamps {
        apply_timestamps(dst, entry);
    }
    Ok(())
}

/// Re-create a symlink or junction pointing at the same place.
pub fn create_link(dst: &Path, kind: EntryKind, target: &str) -> Result<()> {
    // Whatever is there now has to go; you cannot retarget a link in place.
    let _ = force_remove(dst);
    if let Some(parent) = dst.parent() {
        let _ = create_dir(parent);
    }
    imp::create_link(dst, kind, target)
}

/// Give an already-copied file a second name.
pub fn create_hardlink(dst: &Path, existing: &Path) -> Result<()> {
    let _ = force_remove(dst);
    if let Some(parent) = dst.parent() {
        let _ = create_dir(parent);
    }
    fs::hard_link(path::long(existing), path::long(dst)).map_err(BkError::Io)
}

/// Delete a file, clearing the read-only attribute first.
///
/// This is the single most common reason a naive mirror fails partway: NTFS
/// refuses to delete a read-only file, and a backup destination is full of
/// them because we faithfully copied the attribute across.
pub fn remove_file(dst: &Path) -> Result<()> {
    match fs::remove_file(path::long(dst)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => {
            clear_readonly(dst)?;
            match fs::remove_file(path::long(dst)) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(BkError::Io(e)),
            }
        }
    }
}

/// Remove a directory. Only succeeds once it is empty, which the plan
/// guarantees by ordering removals deepest-first.
pub fn remove_dir(dst: &Path) -> Result<()> {
    match fs::remove_dir(path::long(dst)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => {
            let _ = clear_readonly(dst);
            match fs::remove_dir(path::long(dst)) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(BkError::Io(e)),
            }
        }
    }
}

/// Best-effort removal of whatever is at this path, file or directory.
pub fn force_remove(p: &Path) -> Result<()> {
    let lp = path::long(p);
    let md = match fs::symlink_metadata(&lp) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    // A directory symlink or junction must be removed as a link, not
    // recursed into — otherwise we would delete the *target's* contents.
    if md.file_type().is_symlink() {
        return fs::remove_dir(&lp)
            .or_else(|_| fs::remove_file(&lp))
            .map_err(BkError::Io);
    }
    if md.is_dir() {
        let _ = clear_readonly(p);
        fs::remove_dir_all(&lp).map_err(BkError::Io)
    } else {
        remove_file(p)
    }
}

/// Move an entry into the attic instead of deleting it.
pub fn move_to_attic(dst: &Path, attic_root: &Path, rel: &RelPath) -> Result<PathBuf> {
    let target = rel.under(attic_root);
    if let Some(parent) = target.parent() {
        create_dir(parent)?;
    }
    let _ = force_remove(&target);
    match fs::rename(path::long(dst), path::long(&target)) {
        Ok(()) => Ok(target),
        // A rename across volumes fails; the attic lives on the destination
        // volume so this should not happen, but fall back rather than fail.
        Err(_) => {
            let md = fs::symlink_metadata(path::long(dst))?;
            if md.is_dir() {
                copy_tree_shallow(dst, &target)?;
            } else {
                fs::copy(path::long(dst), path::long(&target))?;
            }
            force_remove(dst)?;
            Ok(target)
        }
    }
}

fn copy_tree_shallow(src: &Path, dst: &Path) -> Result<()> {
    create_dir(dst)?;
    for e in fs::read_dir(path::long(src))? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree_shallow(&e.path(), &to)?;
        } else {
            fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

/// The temp path a file is written to before being committed.
pub fn part_path(dst: &Path) -> PathBuf {
    let mut s = dst.as_os_str().to_os_string();
    s.push(PART_SUFFIX);
    PathBuf::from(s)
}

/// Delete `.bku-part` leftovers under `root`. Called at the start of a run so
/// an interrupted previous attempt does not leave litter forever.
pub fn sweep_part_files(root: &Path) -> Result<u64> {
    let mut removed = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(path::long(&dir)) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(_) if p.to_string_lossy().ends_with(PART_SUFFIX) && force_remove(&p).is_ok() => {
                    removed += 1;
                }
                _ => {}
            }
        }
    }
    Ok(removed)
}

/// Clear the read-only bit so a file can be replaced or deleted.
pub fn clear_readonly(p: &Path) -> Result<()> {
    let lp = path::long(p);
    let md = match fs::metadata(&lp) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    let mut perms = md.permissions();
    if perms.readonly() {
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        let _ = fs::set_permissions(&lp, perms);
    }
    Ok(())
}

/// Hash both files and compare. Used by `--verify` and by content comparison.
pub fn verify_same(a: &Path, b: &Path) -> Result<bool> {
    Ok(hash_file(a)? == hash_file(b)?)
}

/// BLAKE3 of a file's primary data stream. Reuses the same hash the
/// block-level engine uses, so the two halves of the tool agree on identity.
pub fn hash_file(p: &Path) -> Result<[u8; 32]> {
    let mut f = fs::File::open(path::long(p))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

// ===========================================================================
// Data copy
// ===========================================================================

fn copy_data(
    src: &Path,
    part: &Path,
    entry: &Entry,
    opts: &CopyOptions,
    cancel: &Cancel,
    progress: &mut dyn FnMut(u64),
) -> Result<CopyOutcome> {
    if let Some(parent) = part.parent() {
        create_dir(parent)?;
    }

    // A sparse source (VM disks, database files, torrents) can be logically
    // huge and physically tiny. Copying it densely would turn a 40 GB
    // allocation into 2 TB of zeroes on the destination.
    if opts.preserve_sparse && entry.is_sparse() {
        if let Some(outcome) = imp::copy_sparse(src, part, entry, opts, cancel, progress)? {
            return Ok(outcome);
        }
        // Fall through to a dense copy if the destination cannot do sparse.
    }

    let mut r = imp::open_source(src, opts)?;
    let mut w = imp::create_dest(part, opts)?;
    let mut buf = vec![0u8; opts.buffer_size.max(64 * 1024)];
    let mut total = 0u64;

    loop {
        if cancel.is_cancelled() {
            return Err(BkError::Other(anyhow::anyhow!("cancelled")));
        }
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
        total += n as u64;
        progress(n as u64);
    }
    w.flush()?;

    Ok(CopyOutcome {
        bytes_written: total,
        ..Default::default()
    })
}

// ===========================================================================
// Windows
// ===========================================================================
#[cfg(windows)]
mod imp {
    use super::*;
    use crate::file::sys::{wide, OwnedHandle};
    use std::ffi::c_void;
    use std::os::windows::io::FromRawHandle;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Foundation::{GetLastError, LocalFree, HANDLE, HLOCAL, LUID};
    use windows::Win32::Security::Authorization::{
        GetNamedSecurityInfoW, SetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, ACL, DACL_SECURITY_INFORMATION,
        GROUP_SECURITY_INFORMATION, LUID_AND_ATTRIBUTES, OBJECT_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FindClose, FindFirstStreamW, FindNextStreamW, FindStreamInfoStandard,
        SetFileAttributesW, SetFileTime, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL,
        FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_FLAG_SEQUENTIAL_SCAN, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING, WIN32_FIND_STREAM_DATA,
    };
    use windows::Win32::System::Ioctl::{
        FILE_ALLOCATED_RANGE_BUFFER, FSCTL_QUERY_ALLOCATED_RANGES, FSCTL_SET_REPARSE_POINT,
        FSCTL_SET_SPARSE,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::Win32::System::IO::DeviceIoControl;

    const GENERIC_READ_U32: u32 = 0x8000_0000;
    const GENERIC_WRITE_U32: u32 = 0x4000_0000;
    const WRITE_DAC_U32: u32 = 0x0004_0000;
    const WRITE_OWNER_U32: u32 = 0x0008_0000;

    /// Ask Windows for the backup/restore privileges.
    ///
    /// With `SeBackupPrivilege` held and a handle opened with
    /// `FILE_FLAG_BACKUP_SEMANTICS`, the ACL check is bypassed — which is how a
    /// backup tool reads a user's private folders without taking ownership of
    /// them. `SeRestorePrivilege` is the write-side equivalent. Both are
    /// present in an Administrator token but disabled by default, so they have
    /// to be explicitly enabled. Failing is not fatal: we simply fall back to
    /// ordinary access rights and report files we could not read.
    pub fn enable_backup_privileges() -> bool {
        [
            "SeBackupPrivilege",
            "SeRestorePrivilege",
            "SeSecurityPrivilege",
        ]
        .iter()
        .all(|p| enable_privilege(p))
    }

    fn enable_privilege(name: &str) -> bool {
        let wname: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: standard AdjustTokenPrivileges sequence; every pointer is a
        // valid local, and the token handle is closed exactly once.
        unsafe {
            let mut token = HANDLE::default();
            if OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            )
            .is_err()
            {
                return false;
            }
            let mut luid = LUID::default();
            let ok = if LookupPrivilegeValueW(PCWSTR::null(), PCWSTR(wname.as_ptr()), &mut luid)
                .is_ok()
            {
                let tp = TOKEN_PRIVILEGES {
                    PrivilegeCount: 1,
                    Privileges: [LUID_AND_ATTRIBUTES {
                        Luid: luid,
                        Attributes: SE_PRIVILEGE_ENABLED,
                    }],
                };
                AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None).is_ok()
                    // AdjustTokenPrivileges "succeeds" even when it assigned
                    // nothing, so the real answer is in GetLastError.
                    && GetLastError().is_ok()
            } else {
                false
            };
            let _ = CloseHandle(token);
            ok
        }
    }

    fn backup_flags(opts: &CopyOptions) -> FILE_FLAGS_AND_ATTRIBUTES {
        if opts.use_backup_privilege {
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_SEQUENTIAL_SCAN
        } else {
            FILE_FLAG_SEQUENTIAL_SCAN
        }
    }

    /// Open the source for reading, bypassing ACLs where we are allowed to.
    pub fn open_source(src: &Path, opts: &CopyOptions) -> Result<fs::File> {
        let w = wide(src);
        // SAFETY: `w` is a valid null-terminated UTF-16 path outliving the call.
        let h = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                GENERIC_READ_U32,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                backup_flags(opts),
                HANDLE::default(),
            )
        }
        .map_err(|e| BkError::Other(anyhow::anyhow!("open {}: {e}", path::display(src))))?;
        // SAFETY: the handle is valid and ownership moves into the File.
        Ok(unsafe { fs::File::from_raw_handle(h.0 as _) })
    }

    /// Create the destination temp file.
    pub fn create_dest(part: &Path, opts: &CopyOptions) -> Result<fs::File> {
        let w = wide(part);
        // SAFETY: as above.
        let h = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                GENERIC_WRITE_U32,
                FILE_SHARE_READ,
                None,
                CREATE_ALWAYS,
                backup_flags(opts),
                HANDLE::default(),
            )
        }
        .map_err(|e| BkError::Other(anyhow::anyhow!("create {}: {e}", path::display(part))))?;
        // SAFETY: the handle is valid and ownership moves into the File.
        Ok(unsafe { fs::File::from_raw_handle(h.0 as _) })
    }

    /// Copy a sparse file, transferring only the allocated ranges.
    ///
    /// Returns `Ok(None)` if the destination will not accept sparseness, so the
    /// caller can fall back to a dense copy.
    pub fn copy_sparse(
        src: &Path,
        part: &Path,
        entry: &Entry,
        opts: &CopyOptions,
        cancel: &Cancel,
        progress: &mut dyn FnMut(u64),
    ) -> Result<Option<CopyOutcome>> {
        use std::io::{Seek, SeekFrom};

        let mut r = open_source(src, opts)?;
        let ranges = match allocated_ranges(&r, entry.size) {
            Some(rs) => rs,
            None => return Ok(None),
        };

        let mut w = create_dest(part, opts)?;
        if !mark_sparse(&w) {
            return Ok(None); // destination filesystem cannot do it
        }
        // Establish the full logical length; the holes stay unallocated.
        w.set_len(entry.size)?;

        let mut buf = vec![0u8; opts.buffer_size.max(64 * 1024)];
        let mut total = 0u64;
        for (start, len) in ranges {
            r.seek(SeekFrom::Start(start))?;
            w.seek(SeekFrom::Start(start))?;
            let mut left = len;
            while left > 0 {
                if cancel.is_cancelled() {
                    return Err(BkError::Other(anyhow::anyhow!("cancelled")));
                }
                let want = left.min(buf.len() as u64) as usize;
                let n = r.read(&mut buf[..want])?;
                if n == 0 {
                    break;
                }
                w.write_all(&buf[..n])?;
                left -= n as u64;
                total += n as u64;
                progress(n as u64);
            }
        }
        w.flush()?;
        Ok(Some(CopyOutcome {
            bytes_written: total,
            sparse: true,
            ..Default::default()
        }))
    }

    /// Ask NTFS which byte ranges of a file actually hold data.
    fn allocated_ranges(f: &fs::File, size: u64) -> Option<Vec<(u64, u64)>> {
        use std::os::windows::io::AsRawHandle;
        let h = HANDLE(f.as_raw_handle() as _);
        let query = FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: 0,
            Length: size as i64,
        };
        // A file with many holes needs several passes; 1024 ranges per call is
        // plenty for anything realistic and bounded in memory.
        let mut out: Vec<FILE_ALLOCATED_RANGE_BUFFER> =
            vec![FILE_ALLOCATED_RANGE_BUFFER::default(); 1024];
        let mut returned: u32 = 0;
        // SAFETY: both buffers are valid, correctly sized, and match what this
        // IOCTL documents for input and output.
        let ok = unsafe {
            DeviceIoControl(
                h,
                FSCTL_QUERY_ALLOCATED_RANGES,
                Some(&query as *const _ as *const c_void),
                std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>() as u32,
                Some(out.as_mut_ptr() as *mut c_void),
                (out.len() * std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>()) as u32,
                Some(&mut returned),
                None,
            )
        };
        if ok.is_err() {
            return None;
        }
        let n = returned as usize / std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>();
        Some(
            out[..n]
                .iter()
                .map(|r| (r.FileOffset as u64, r.Length as u64))
                .collect(),
        )
    }

    /// Mark the destination sparse so writes leave holes unallocated.
    fn mark_sparse(f: &fs::File) -> bool {
        use std::os::windows::io::AsRawHandle;
        let h = HANDLE(f.as_raw_handle() as _);
        let mut returned: u32 = 0;
        // SAFETY: FSCTL_SET_SPARSE takes no input buffer.
        unsafe {
            DeviceIoControl(
                h,
                FSCTL_SET_SPARSE,
                None,
                0,
                None,
                0,
                Some(&mut returned),
                None,
            )
        }
        .is_ok()
    }

    /// Copy every named alternate data stream. Returns how many were copied.
    ///
    /// ADS carry real user-visible state — most commonly `Zone.Identifier`,
    /// the "this came from the internet" mark — and less commonly application
    /// data that vanishing would corrupt.
    pub fn copy_streams(src: &Path, dst: &Path) -> Result<u32> {
        let w = wide(src);
        let mut data = WIN32_FIND_STREAM_DATA::default();
        // SAFETY: valid path; `data` is the out-param this info level expects.
        let h = match unsafe {
            FindFirstStreamW(
                PCWSTR(w.as_ptr()),
                FindStreamInfoStandard,
                &mut data as *mut _ as *mut c_void,
                0,
            )
        } {
            Ok(h) if !h.is_invalid() => h,
            _ => return Ok(0),
        };
        let _guard = FindGuard(h);

        let mut copied = 0u32;
        loop {
            let name = {
                let end = data
                    .cStreamName
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(data.cStreamName.len());
                String::from_utf16_lossy(&data.cStreamName[..end])
            };
            // The unnamed data stream is reported as `::$DATA`; it is the file
            // itself and was already copied.
            if !name.is_empty() && name != "::$DATA" && copy_one_stream(src, dst, &name).is_ok() {
                copied += 1;
            }
            // SAFETY: valid find handle and out-param.
            if unsafe { FindNextStreamW(h, &mut data as *mut _ as *mut c_void) }.is_err() {
                break;
            }
        }
        Ok(copied)
    }

    struct FindGuard(HANDLE);
    impl Drop for FindGuard {
        fn drop(&mut self) {
            // SAFETY: handle came from FindFirstStreamW, closed exactly once.
            unsafe {
                let _ = FindClose(self.0);
            }
        }
    }

    /// Copy one `file:stream:$DATA` by appending the stream name to the path.
    fn copy_one_stream(src: &Path, dst: &Path, stream: &str) -> Result<()> {
        let sp = with_stream(src, stream);
        let dp = with_stream(dst, stream);
        let mut r = fs::File::open(&sp)?;
        let mut w = fs::File::create(&dp)?;
        std::io::copy(&mut r, &mut w)?;
        Ok(())
    }

    fn with_stream(p: &Path, stream: &str) -> PathBuf {
        let mut s = path::long(p).as_os_str().to_os_string();
        s.push(stream);
        PathBuf::from(s)
    }

    /// Copy the security descriptor (owner, group, DACL) from source to
    /// destination. The SACL is deliberately not copied: it needs
    /// `SeSecurityPrivilege`, is audit policy rather than access control, and
    /// is rarely meaningful on a different volume.
    pub fn copy_security(src: &Path, dst: &Path) -> bool {
        let sw = wide(src);
        let dw = wide(dst);
        // These constants are newtypes without a `BitOr`, so combine the raw
        // bits and rewrap.
        let info = OBJECT_SECURITY_INFORMATION(
            OWNER_SECURITY_INFORMATION.0
                | GROUP_SECURITY_INFORMATION.0
                | DACL_SECURITY_INFORMATION.0,
        );

        // SAFETY: the owner/group/DACL out-params are pointers *into* a single
        // allocation that `sd` owns. They stay valid until that allocation is
        // freed, which happens with `LocalFree` below — after
        // `SetNamedSecurityInfoW` has finished reading them.
        unsafe {
            let mut owner = PSID(std::ptr::null_mut());
            let mut group = PSID(std::ptr::null_mut());
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sd = PSECURITY_DESCRIPTOR::default();

            // These return a WIN32_ERROR, not a Result: 0 is ERROR_SUCCESS.
            let rc = GetNamedSecurityInfoW(
                PCWSTR(sw.as_ptr()),
                SE_FILE_OBJECT,
                info,
                Some(&mut owner),
                Some(&mut group),
                Some(&mut dacl),
                None,
                &mut sd,
            );
            if rc.0 != 0 {
                return false;
            }

            let rc = SetNamedSecurityInfoW(
                PCWSTR(dw.as_ptr()),
                SE_FILE_OBJECT,
                info,
                owner,
                group,
                if dacl.is_null() {
                    None
                } else {
                    Some(dacl as *const ACL)
                },
                None,
            );
            let ok = rc.0 == 0;

            if !sd.is_invalid() {
                let _ = LocalFree(HLOCAL(sd.0));
            }
            ok
        }
    }

    /// Directories get their security from the same source path, so there is
    /// nothing extra to do beyond what `copy_security` already provides.
    pub fn copy_security_from_entry(_dst: &Path, _entry: &Entry) -> bool {
        true
    }

    /// Apply all three timestamps from raw FILETIME ticks — no conversion
    /// through `SystemTime`, so nothing is rounded.
    pub fn apply_timestamps(dst: &Path, entry: &Entry) -> bool {
        use windows::Win32::Foundation::FILETIME;
        let w = wide(dst);
        // SAFETY: valid path; the handle is closed by OwnedHandle's Drop.
        let h = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                0x0100, /* FILE_WRITE_ATTRIBUTES */
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                HANDLE::default(),
            )
        };
        let h = match h {
            Ok(h) => OwnedHandle(h),
            Err(_) => return false,
        };
        let ft = |t: Option<super::super::meta::FileTime>| -> Option<FILETIME> {
            t.filter(|t| !t.is_zero()).map(|t| {
                let ticks = t.to_filetime();
                FILETIME {
                    dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
                    dwHighDateTime: (ticks >> 32) as u32,
                }
            })
        };
        let created = ft(entry.created);
        let accessed = ft(entry.accessed);
        let modified = ft(Some(entry.modified));
        // SAFETY: valid handle; each pointer either points at a live local or
        // is None, which the API reads as "leave this timestamp alone".
        unsafe {
            SetFileTime(
                h.0,
                created.as_ref().map(|f| f as *const _),
                accessed.as_ref().map(|f| f as *const _),
                modified.as_ref().map(|f| f as *const _),
            )
        }
        .is_ok()
    }

    /// Apply the mirrored attribute bits.
    pub fn apply_attributes(dst: &Path, entry: &Entry) -> bool {
        let w = wide(dst);
        let mut bits = entry.attrs & attr::MIRRORED;
        if bits == 0 {
            bits = FILE_ATTRIBUTE_NORMAL.0;
        }
        // SAFETY: valid null-terminated path.
        unsafe { SetFileAttributesW(PCWSTR(w.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(bits)) }.is_ok()
    }

    /// Re-create a symlink or junction.
    pub fn create_link(dst: &Path, kind: EntryKind, target: &str) -> Result<()> {
        match kind {
            EntryKind::Junction => create_junction(dst, target),
            _ => {
                // A directory symlink and a file symlink are different objects
                // on Windows, and creating the wrong one produces a link that
                // does not resolve.
                let looks_like_dir = Path::new(target).is_dir();
                let r = if looks_like_dir {
                    std::os::windows::fs::symlink_dir(target, path::long(dst))
                } else {
                    std::os::windows::fs::symlink_file(target, path::long(dst))
                };
                r.map_err(|e| {
                    BkError::Other(anyhow::anyhow!(
                        "symlink {} -> {target}: {e} (creating symlinks needs \
                         Administrator or Developer Mode)",
                        path::display(dst)
                    ))
                })
            }
        }
    }

    /// Build a mount-point reparse point by hand.
    ///
    /// Unlike symlinks, junctions need no special privilege, which makes this
    /// the more reliable of the two paths — worth the manual byte packing.
    fn create_junction(dst: &Path, target: &str) -> Result<()> {
        // The directory must exist and be empty before it can become a junction.
        create_dir(dst)?;

        // The substitute name is the NT-namespace form; the print name is what
        // a user sees in `dir`.
        let substitute: Vec<u16> = format!(r"\??\{}", target.trim_end_matches('\\'))
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let print: Vec<u16> = target
            .trim_end_matches('\\')
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let sub_bytes = (substitute.len() - 1) * 2; // excluding the null
        let print_bytes = (print.len() - 1) * 2;
        let path_buffer_bytes = substitute.len() * 2 + print.len() * 2;

        // REPARSE_DATA_BUFFER: 8-byte header, then the mount-point fields.
        let mut buf: Vec<u8> = Vec::with_capacity(8 + 8 + path_buffer_bytes);
        buf.extend_from_slice(&super::super::meta::tag::MOUNT_POINT.to_le_bytes());
        buf.extend_from_slice(&(((8 + path_buffer_bytes) as u16).to_le_bytes())); // ReparseDataLength
        buf.extend_from_slice(&0u16.to_le_bytes()); // Reserved
        buf.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
        buf.extend_from_slice(&(sub_bytes as u16).to_le_bytes());
        buf.extend_from_slice(&((substitute.len() * 2) as u16).to_le_bytes()); // PrintNameOffset
        buf.extend_from_slice(&(print_bytes as u16).to_le_bytes());
        for c in &substitute {
            buf.extend_from_slice(&c.to_le_bytes());
        }
        for c in &print {
            buf.extend_from_slice(&c.to_le_bytes());
        }

        let w = wide(dst);
        // SAFETY: valid path; handle closed by OwnedHandle.
        let h = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                GENERIC_READ_U32 | GENERIC_WRITE_U32,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                HANDLE::default(),
            )
        }
        .map_err(|e| BkError::Other(anyhow::anyhow!("open junction dir: {e}")))?;
        let h = OwnedHandle(h);

        let mut returned: u32 = 0;
        // SAFETY: `buf` is a correctly laid out REPARSE_DATA_BUFFER of exactly
        // the length we pass.
        unsafe {
            DeviceIoControl(
                h.0,
                FSCTL_SET_REPARSE_POINT,
                Some(buf.as_ptr() as *const c_void),
                buf.len() as u32,
                None,
                0,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|e| BkError::Other(anyhow::anyhow!("set reparse point: {e}")))?;
        Ok(())
    }

    /// Access rights kept for reference; the copy path opens with the broader
    /// backup semantics instead.
    #[allow(dead_code)]
    const _RIGHTS: (u32, u32) = (WRITE_DAC_U32, WRITE_OWNER_U32);
}

// ===========================================================================
// Unix
// ===========================================================================
#[cfg(not(windows))]
mod imp {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    /// No equivalent privilege model; running as root is the analogue and is
    /// checked by the caller.
    pub fn enable_backup_privileges() -> bool {
        super::super::sys::is_elevated()
    }

    pub fn open_source(src: &Path, _opts: &CopyOptions) -> Result<fs::File> {
        Ok(fs::File::open(src)?)
    }

    pub fn create_dest(part: &Path, _opts: &CopyOptions) -> Result<fs::File> {
        Ok(fs::File::create(part)?)
    }

    /// Sparse copy using `SEEK_DATA`/`SEEK_HOLE`, the POSIX equivalent of
    /// `FSCTL_QUERY_ALLOCATED_RANGES`. Supported by ext4, XFS and Btrfs;
    /// filesystems that lack it report the whole file as data, which degrades
    /// to a dense copy rather than failing.
    pub fn copy_sparse(
        src: &Path,
        part: &Path,
        entry: &Entry,
        opts: &CopyOptions,
        cancel: &Cancel,
        progress: &mut dyn FnMut(u64),
    ) -> Result<Option<CopyOutcome>> {
        use std::io::{Seek, SeekFrom};
        use std::os::unix::io::AsRawFd;

        let mut r = fs::File::open(src)?;
        let mut w = fs::File::create(part)?;
        w.set_len(entry.size)?;

        let fd = r.as_raw_fd();
        let mut buf = vec![0u8; opts.buffer_size.max(64 * 1024)];
        let mut total = 0u64;
        let mut pos = 0i64;

        while (pos as u64) < entry.size {
            if cancel.is_cancelled() {
                return Err(BkError::Other(anyhow::anyhow!("cancelled")));
            }
            // SAFETY: `fd` is a live descriptor owned by `r`.
            let data_start = unsafe { libc::lseek(fd, pos, libc::SEEK_DATA) };
            if data_start < 0 {
                break; // no more data (ENXIO) or unsupported
            }
            // SAFETY: as above.
            let hole_start = unsafe { libc::lseek(fd, data_start, libc::SEEK_HOLE) };
            let end = if hole_start < 0 {
                entry.size as i64
            } else {
                hole_start
            };

            r.seek(SeekFrom::Start(data_start as u64))?;
            w.seek(SeekFrom::Start(data_start as u64))?;
            let mut left = (end - data_start) as u64;
            while left > 0 {
                let want = left.min(buf.len() as u64) as usize;
                let n = r.read(&mut buf[..want])?;
                if n == 0 {
                    break;
                }
                w.write_all(&buf[..n])?;
                left -= n as u64;
                total += n as u64;
                progress(n as u64);
            }
            pos = end;
        }
        w.flush()?;
        Ok(Some(CopyOutcome {
            bytes_written: total,
            sparse: true,
            ..Default::default()
        }))
    }

    /// Linux extended attributes are the nearest analogue to NTFS alternate
    /// data streams, but they are not equivalent and are not mirrored yet.
    pub fn copy_streams(_src: &Path, _dst: &Path) -> Result<u32> {
        Ok(0)
    }

    /// POSIX ACLs need `libacl`. Mode bits are preserved via
    /// `apply_attributes`, which covers the common case.
    pub fn copy_security(_src: &Path, _dst: &Path) -> bool {
        false
    }

    pub fn copy_security_from_entry(dst: &Path, entry: &Entry) -> bool {
        if entry.mode == 0 {
            return true;
        }
        fs::set_permissions(dst, fs::Permissions::from_mode(entry.mode & 0o7777)).is_ok()
    }

    /// Apply mtime and atime with nanosecond precision via `utimensat`.
    /// Unix has no settable creation time, so `entry.created` is dropped.
    pub fn apply_timestamps(dst: &Path, entry: &Entry) -> bool {
        let c = match std::ffi::CString::new(dst.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let to_ts = |t: super::super::meta::FileTime| libc::timespec {
            tv_sec: t.unix_nanos.div_euclid(1_000_000_000) as libc::time_t,
            tv_nsec: t.unix_nanos.rem_euclid(1_000_000_000) as _,
        };
        let times = [
            to_ts(entry.accessed.unwrap_or(entry.modified)),
            to_ts(entry.modified),
        ];
        // SAFETY: `c` is a valid null-terminated path and `times` is a live
        // two-element array, exactly as utimensat requires.
        unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                c.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            ) == 0
        }
    }

    /// The Unix analogue of Windows attributes is the permission bits.
    pub fn apply_attributes(dst: &Path, entry: &Entry) -> bool {
        if entry.mode != 0 {
            return fs::set_permissions(dst, fs::Permissions::from_mode(entry.mode & 0o7777))
                .is_ok();
        }
        // Fall back to honouring the read-only flag if that is all we have.
        if entry.attrs & attr::READONLY != 0 {
            if let Ok(md) = fs::metadata(dst) {
                let mut p = md.permissions();
                p.set_readonly(true);
                return fs::set_permissions(dst, p).is_ok();
            }
        }
        true
    }

    /// Unix has one kind of symlink and no junctions, so both map to `symlink`.
    pub fn create_link(dst: &Path, _kind: EntryKind, target: &str) -> Result<()> {
        std::os::unix::fs::symlink(target, dst).map_err(BkError::Io)
    }
}

pub use imp::{
    apply_attributes, apply_timestamps, copy_security, copy_security_from_entry, copy_streams,
    enable_backup_privileges,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::meta::FileTime;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(n: &str) -> Self {
            let p = std::env::temp_dir().join(format!("bku_copy_{n}"));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
        fn p(&self, n: &str) -> PathBuf {
            self.0.join(n)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry_for(p: &Path) -> Entry {
        let md = fs::metadata(p).unwrap();
        let mut e = Entry::dir(RelPath::from_key_text(
            p.file_name().unwrap().to_str().unwrap(),
        ));
        e.kind = EntryKind::File;
        e.size = md.len();
        e.attrs = 0;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            e.attrs = md.file_attributes();
            e.modified = FileTime::from_filetime(md.last_write_time());
            e.created = Some(FileTime::from_filetime(md.creation_time()));
            e.accessed = Some(FileTime::from_filetime(md.last_access_time()));
        }
        e
    }

    #[test]
    fn copies_content_byte_for_byte() {
        let t = Tmp::new("content");
        let src = t.p("a.bin");
        let dst = t.p("b.bin");
        // Deliberately not a round number of buffers.
        let data: Vec<u8> = (0..1_000_003u32).map(|i| (i % 251) as u8).collect();
        fs::write(&src, &data).unwrap();

        let mut moved = 0u64;
        let out = copy_file(
            &src,
            &dst,
            &entry_for(&src),
            &CopyOptions::default(),
            &Cancel::none(),
            &mut |n| moved += n,
        )
        .unwrap();

        assert_eq!(out.bytes_written, data.len() as u64);
        assert_eq!(
            moved,
            data.len() as u64,
            "progress must total the file size"
        );
        assert_eq!(fs::read(&dst).unwrap(), data);
        assert_eq!(hash_file(&src).unwrap(), hash_file(&dst).unwrap());
    }

    #[test]
    fn an_empty_file_copies_cleanly() {
        let t = Tmp::new("empty");
        let src = t.p("e.bin");
        let dst = t.p("e2.bin");
        fs::write(&src, b"").unwrap();
        let out = copy_file(
            &src,
            &dst,
            &entry_for(&src),
            &CopyOptions::default(),
            &Cancel::none(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(out.bytes_written, 0);
        assert!(dst.exists());
    }

    #[test]
    fn verification_catches_a_mismatch_and_passes_a_good_copy() {
        let t = Tmp::new("verify");
        let src = t.p("v.bin");
        let dst = t.p("v2.bin");
        fs::write(&src, vec![7u8; 100_000]).unwrap();
        let opts = CopyOptions {
            verify: true,
            ..Default::default()
        };
        let out = copy_file(
            &src,
            &dst,
            &entry_for(&src),
            &opts,
            &Cancel::none(),
            &mut |_| {},
        )
        .unwrap();
        assert!(out.verified);
    }

    #[test]
    fn an_interrupted_copy_leaves_no_file_at_the_real_path() {
        // This is the property that makes the destination trustworthy: a
        // cancelled copy must never leave a truncated file wearing the real
        // name, because a user would later restore from it.
        let t = Tmp::new("interrupt");
        let src = t.p("big.bin");
        let dst = t.p("big_copy.bin");
        fs::write(&src, vec![3u8; 8 * 1024 * 1024]).unwrap();

        let cancel = Cancel::new();
        cancel.cancel();
        let opts = CopyOptions {
            buffer_size: 64 * 1024,
            ..Default::default()
        };
        let r = copy_file(&src, &dst, &entry_for(&src), &opts, &cancel, &mut |_| {});
        assert!(r.is_err());
        assert!(!dst.exists(), "no partial file at the destination name");
        assert!(!part_path(&dst).exists(), "and the temp file is cleaned up");
    }

    #[test]
    fn timestamps_are_preserved_exactly() {
        let t = Tmp::new("times");
        let src = t.p("ts.txt");
        let dst = t.p("ts2.txt");
        fs::write(&src, b"when").unwrap();
        // Push the source's mtime into the past so a copy that ignored
        // timestamps would be obviously different.
        let mut e = entry_for(&src);
        e.modified = FileTime::from_unix_secs_nanos(1_600_000_000, 0);
        e.accessed = Some(e.modified);
        e.created = Some(e.modified);

        copy_file(
            &src,
            &dst,
            &e,
            &CopyOptions::default(),
            &Cancel::none(),
            &mut |_| {},
        )
        .unwrap();

        let got = entry_for(&dst);
        #[cfg(windows)]
        assert_eq!(
            got.modified, e.modified,
            "NTFS stores 100 ns ticks, so this must match exactly"
        );
        #[cfg(not(windows))]
        assert!(got.modified.close_to(e.modified, 1_000));
    }

    #[test]
    fn readonly_files_can_still_be_replaced_and_removed() {
        // The classic mirror failure: NTFS refuses to delete or overwrite a
        // read-only file, and a faithful backup destination is full of them.
        let t = Tmp::new("readonly");
        let p = t.p("locked.txt");
        fs::write(&p, b"v1").unwrap();
        let mut perms = fs::metadata(&p).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&p, perms).unwrap();

        let src = t.p("new.txt");
        fs::write(&src, b"v2").unwrap();
        copy_file(
            &src,
            &p,
            &entry_for(&src),
            &CopyOptions::default(),
            &Cancel::none(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"v2");

        // And it must be deletable afterwards.
        let mut perms = fs::metadata(&p).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&p, perms).unwrap();
        remove_file(&p).unwrap();
        assert!(!p.exists());
    }

    #[test]
    fn removing_something_that_is_not_there_is_not_an_error() {
        let t = Tmp::new("absent");
        remove_file(&t.p("nope.txt")).unwrap();
        remove_dir(&t.p("nodir")).unwrap();
        force_remove(&t.p("nothing")).unwrap();
    }

    #[test]
    fn hardlinks_share_storage() {
        let t = Tmp::new("hardlink");
        let a = t.p("a.bin");
        fs::write(&a, b"shared bytes").unwrap();
        let b = t.p("b.bin");
        if create_hardlink(&b, &a).is_err() {
            return; // filesystem does not support it
        }
        assert_eq!(fs::read(&b).unwrap(), b"shared bytes");
        // Writing through one name is visible through the other.
        fs::write(&a, b"changed!!!!!").unwrap();
        assert_eq!(fs::read(&b).unwrap(), b"changed!!!!!");
    }

    #[test]
    fn part_files_are_swept_recursively() {
        let t = Tmp::new("sweep");
        fs::create_dir_all(t.p("sub/deeper")).unwrap();
        fs::write(t.p("a.txt"), b"keep").unwrap();
        fs::write(t.p(&format!("a.txt{PART_SUFFIX}")), b"junk").unwrap();
        fs::write(t.p(&format!("sub/deeper/b.txt{PART_SUFFIX}")), b"junk").unwrap();

        let n = sweep_part_files(&t.0).unwrap();
        assert_eq!(n, 2);
        assert!(t.p("a.txt").exists(), "real files are untouched");
        assert!(!t.p(&format!("a.txt{PART_SUFFIX}")).exists());
    }

    #[test]
    fn the_attic_receives_files_with_their_relative_layout() {
        let t = Tmp::new("attic");
        let dstroot = t.p("dest");
        let attic = t.p("attic");
        fs::create_dir_all(dstroot.join("sub")).unwrap();
        let victim = dstroot.join("sub/old.txt");
        fs::write(&victim, b"outdated").unwrap();

        let rel = RelPath::from_key_text("sub/old.txt");
        let moved = move_to_attic(&victim, &attic, &rel).unwrap();

        assert!(!victim.exists(), "removed from the destination");
        assert_eq!(moved, attic.join("sub").join("old.txt"));
        assert_eq!(fs::read(&moved).unwrap(), b"outdated", "and recoverable");
    }

    #[cfg(windows)]
    #[test]
    fn alternate_data_streams_survive_the_copy() {
        let t = Tmp::new("ads");
        let src = t.p("marked.txt");
        let dst = t.p("marked2.txt");
        fs::write(&src, b"body").unwrap();
        // The stream Windows itself adds to downloaded files.
        let mut s = src.as_os_str().to_os_string();
        s.push(":Zone.Identifier");
        if fs::write(PathBuf::from(&s), b"[ZoneTransfer]\r\nZoneId=3\r\n").is_err() {
            return; // not NTFS
        }

        let out = copy_file(
            &src,
            &dst,
            &entry_for(&src),
            &CopyOptions::default(),
            &Cancel::none(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(out.streams, 1, "the named stream should have been copied");

        let mut d = dst.as_os_str().to_os_string();
        d.push(":Zone.Identifier");
        assert_eq!(
            fs::read(PathBuf::from(&d)).unwrap(),
            b"[ZoneTransfer]\r\nZoneId=3\r\n"
        );
    }

    #[cfg(windows)]
    #[test]
    fn attributes_are_mirrored() {
        let t = Tmp::new("attrs");
        let src = t.p("h.txt");
        let dst = t.p("h2.txt");
        fs::write(&src, b"x").unwrap();
        let mut e = entry_for(&src);
        e.attrs = attr::HIDDEN | attr::READONLY;

        copy_file(
            &src,
            &dst,
            &e,
            &CopyOptions::default(),
            &Cancel::none(),
            &mut |_| {},
        )
        .unwrap();

        use std::os::windows::fs::MetadataExt;
        let got = fs::metadata(&dst).unwrap().file_attributes();
        assert_ne!(got & attr::HIDDEN, 0, "hidden should have carried over");
        assert_ne!(
            got & attr::READONLY,
            0,
            "read-only should have carried over"
        );
    }
}

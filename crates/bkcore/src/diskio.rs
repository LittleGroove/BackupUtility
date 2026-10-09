//! Aligned, unbuffered reads of a volume/shadow device (or, for testing, a
//! plain file).
//!
//! We open with `FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN` so a
//! full-volume read doesn't evict the system file cache. `NO_BUFFERING` demands
//! that every read's offset, length, and *buffer address* be multiples of the
//! physical sector size — hence [`SECTOR_ALIGN`](crate::SECTOR_ALIGN) and the
//! [`AlignedBuf`] below.
//!
//! Length is queried via `IOCTL_DISK_GET_LENGTH_INFO` for real devices, falling
//! back to `GetFileSizeEx` for a regular file (our test path).
//!
//! Note: `NO_BUFFERING` cannot read a trailing partial sector, so a source must
//! be a whole number of sectors. Real volumes/disks always are; test images
//! must be sized to a multiple of `SECTOR_ALIGN`.

use crate::{BkError, Result, CHUNK_SIZE, SECTOR_ALIGN};
use std::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

#[cfg(windows)]
use std::ffi::c_void;
#[cfg(windows)]
use windows::core::PCWSTR;
#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFileSizeEx, ReadFile, SetFilePointerEx, WriteFile, FILE_BEGIN,
    FILE_FLAG_NO_BUFFERING, FILE_FLAG_SEQUENTIAL_SCAN, FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
#[cfg(windows)]
use windows::Win32::System::Ioctl::{GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO};
#[cfg(windows)]
use windows::Win32::System::IO::DeviceIoControl;

#[cfg(windows)]
const GENERIC_READ_U32: u32 = 0x8000_0000;
#[cfg(windows)]
const GENERIC_WRITE_U32: u32 = 0x4000_0000;

/// A heap buffer whose start address is aligned to `SECTOR_ALIGN`, as required
/// by unbuffered I/O. A plain `Vec<u8>` only guarantees 1-byte alignment, which
/// `NO_BUFFERING` rejects.
pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
    layout: Layout,
}

impl AlignedBuf {
    /// Allocate `len` zeroed, sector-aligned bytes. `len` must be a multiple of
    /// `SECTOR_ALIGN`.
    pub fn new(len: usize) -> Self {
        assert!(
            len.is_multiple_of(SECTOR_ALIGN),
            "buffer length must be sector-aligned"
        );
        let layout = Layout::from_size_align(len, SECTOR_ALIGN).expect("valid layout");
        // SAFETY: layout has non-zero size (callers pass a positive multiple).
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        Self { ptr, len, layout }
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr/len describe a single valid allocation we own.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above; &mut self guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: ptr/layout are exactly what alloc_zeroed returned.
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

// The buffer is a plain owned allocation; moving it across threads is safe.
unsafe impl Send for AlignedBuf {}

/// A sequential reader over a device path (a real volume, a VSS shadow, or a
/// regular file for testing).
#[cfg(windows)]
pub struct SectorReader {
    handle: HANDLE,
    len: u64,
    pos: u64,
}

#[cfg(windows)]
impl SectorReader {
    /// Open `device_path` for aligned, unbuffered sequential reading.
    pub fn open(device_path: &str) -> Result<Self> {
        let wide: Vec<u16> = device_path
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: wide is a valid, null-terminated UTF-16 string that outlives
        // the call; all other args are plain values.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ_U32,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN,
                HANDLE::default(),
            )
        }
        .map_err(|e| win_err("CreateFileW", e))?;

        let len = query_length(handle)?;
        Ok(Self {
            handle,
            len,
            pos: 0,
        })
    }

    /// Total size of the device/file in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Fill `buf` from the current position, returning the number of bytes read.
    /// Loops internally so a returned window is always full except at EOF, which
    /// keeps chunk offsets aligned across windows. `buf.len()` must be a
    /// multiple of `SECTOR_ALIGN`.
    pub fn read_window(&mut self, buf: &mut [u8]) -> Result<usize> {
        debug_assert!(buf.len().is_multiple_of(SECTOR_ALIGN));
        // Never request past the end of the device. Unlike a file (which
        // short-reads at EOF), a raw device returns ERROR_SECTOR_NOT_FOUND if
        // asked for sectors beyond its size. Clamp to the sector-aligned
        // remainder so the final window reads exactly what's left.
        let remaining = self.len.saturating_sub(self.pos);
        if remaining == 0 {
            return Ok(0);
        }
        let mut want = std::cmp::min(buf.len() as u64, remaining) as usize;
        want -= want % SECTOR_ALIGN; // keep unbuffered I/O legal
        if want == 0 {
            return Ok(0); // sub-sector tail is unreadable unbuffered
        }

        let mut filled = 0usize;
        while filled < want {
            let mut got: u32 = 0;
            let slice = &mut buf[filled..want];
            // SAFETY: slice start stays sector-aligned (filled is always a
            // sector multiple), and `got` is a valid out-param.
            unsafe { ReadFile(self.handle, Some(slice), Some(&mut got), None) }
                .map_err(|e| win_err("ReadFile", e))?;
            if got == 0 {
                break; // EOF
            }
            filled += got as usize;
            self.pos += got as u64;
        }
        Ok(filled)
    }
}

#[cfg(windows)]
impl Drop for SectorReader {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: handle came from CreateFileW and is closed exactly once.
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }
}

/// Query the byte length of an open handle: try the disk IOCTL (devices), fall
/// back to file size (regular files).
#[cfg(windows)]
fn query_length(handle: HANDLE) -> Result<u64> {
    let mut info = GET_LENGTH_INFORMATION::default();
    let mut returned: u32 = 0;
    // SAFETY: out buffer matches the documented output struct for this IOCTL.
    let ioctl = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_LENGTH_INFO,
            None,
            0,
            Some(&mut info as *mut _ as *mut c_void),
            std::mem::size_of::<GET_LENGTH_INFORMATION>() as u32,
            Some(&mut returned),
            None,
        )
    };
    if ioctl.is_ok() {
        return Ok(info.Length as u64);
    }

    // Regular file: the IOCTL isn't supported, so ask for the file size.
    let mut size: i64 = 0;
    // SAFETY: size is a valid out-param.
    unsafe { GetFileSizeEx(handle, &mut size) }.map_err(|e| win_err("GetFileSizeEx", e))?;
    Ok(size as u64)
}

#[cfg(windows)]
fn win_err(ctx: &str, e: windows::core::Error) -> BkError {
    BkError::Other(anyhow::anyhow!("{ctx} failed: {e}"))
}

// ===========================================================================
// Write / apply path — the shared core for BOTH image-restore and drive-clone.
//
// `BlockTarget` is a seekable place we write reconstructed chunks into:
//   - `FileTarget`   -> a file image (restore, and our no-admin test path).
//   - `DeviceTarget` -> a raw disk `\\.\PhysicalDriveN` (clone; needs admin).
//
// The apply loop in `bkcore::apply` writes chunks by offset and, for an
// *incremental clone*, can `read_at` the target first to skip blocks already
// identical — so a re-clone only writes what changed.
// ===========================================================================

/// A random-access block sink we can reconstruct a volume into.
pub trait BlockTarget {
    /// Capacity in bytes (file length, or device size).
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Read up to `buf.len()` bytes at `offset`; returns bytes read (short only
    /// at EOF). Used by incremental clone to compare before writing.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize>;
    /// Write all of `data` at `offset`.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()>;
    /// Flush to durable storage.
    fn flush(&mut self) -> Result<()>;
}

/// A file-backed target. Buffered std I/O — portable, no alignment rules, no
/// admin. Used for image restore and for the round-trip tests.
pub struct FileTarget {
    f: File,
    len: u64,
}

impl FileTarget {
    /// Create (or truncate) a file image of exactly `size` bytes.
    pub fn create(path: impl AsRef<Path>, size: u64) -> Result<Self> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        f.set_len(size)?;
        Ok(Self { f, len: size })
    }

    /// Open an existing file image read/write (for incremental re-apply).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let f = OpenOptions::new().read(true).write(true).open(path)?;
        let len = f.metadata()?.len();
        Ok(Self { f, len })
    }
}

impl BlockTarget for FileTarget {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.f.seek(SeekFrom::Start(offset))?;
        let mut filled = 0;
        while filled < buf.len() {
            let n = self.f.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        Ok(filled)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.f.seek(SeekFrom::Start(offset))?;
        self.f.write_all(data)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.f.flush()?;
        self.f.sync_all()?;
        Ok(())
    }
}

/// A raw-disk target: writes straight to `\\.\PhysicalDriveN` (or a volume).
/// Unbuffered + write-through, so writes hit the device, not a cache. Offsets
/// and lengths must be sector-aligned; the apply loop guarantees this because
/// every chunk starts at a `CHUNK_SIZE` multiple and (on a whole-sector source)
/// has a sector-multiple length. A sector-aligned scratch buffer bridges the
/// unaligned `Vec<u8>` chunk bytes to the aligned I/O the device demands.
///
/// DESTRUCTIVE: opening for write and writing will overwrite the target disk.
#[cfg(windows)]
pub struct DeviceTarget {
    handle: HANDLE,
    len: u64,
    scratch: AlignedBuf,
}

#[cfg(windows)]
impl DeviceTarget {
    /// Open an existing device for read+write, unbuffered write-through.
    pub fn open_device(device_path: &str) -> Result<Self> {
        let wide: Vec<u16> = device_path
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: valid null-terminated path; plain-value flags.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ_U32 | GENERIC_WRITE_U32,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
                HANDLE::default(),
            )
        }
        .map_err(|e| win_err("CreateFileW(device,write)", e))?;

        let len = query_length(handle)?;
        Ok(Self {
            handle,
            len,
            scratch: AlignedBuf::new(CHUNK_SIZE),
        })
    }

    fn seek(&self, offset: u64) -> Result<()> {
        // SAFETY: valid handle; offset within device.
        unsafe { SetFilePointerEx(self.handle, offset as i64, None, FILE_BEGIN) }
            .map_err(|e| win_err("SetFilePointerEx", e))
    }
}

#[cfg(windows)]
impl BlockTarget for DeviceTarget {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        debug_assert!(buf.len().is_multiple_of(SECTOR_ALIGN) && buf.len() <= self.scratch.len());
        self.seek(offset)?;
        let mut got: u32 = 0;
        let slice = &mut self.scratch.as_mut_slice()[..buf.len()];
        // SAFETY: aligned scratch slice, valid out-param.
        unsafe { ReadFile(self.handle, Some(slice), Some(&mut got), None) }
            .map_err(|e| win_err("ReadFile(device)", e))?;
        let n = got as usize;
        buf[..n].copy_from_slice(&self.scratch.as_slice()[..n]);
        Ok(n)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        debug_assert!(data.len().is_multiple_of(SECTOR_ALIGN) && data.len() <= self.scratch.len());
        // Copy into the aligned scratch buffer that NO_BUFFERING requires.
        self.scratch.as_mut_slice()[..data.len()].copy_from_slice(data);
        self.seek(offset)?;
        let mut written: u32 = 0;
        let slice = &self.scratch.as_slice()[..data.len()];
        // SAFETY: aligned scratch slice, valid out-param.
        unsafe { WriteFile(self.handle, Some(slice), Some(&mut written), None) }
            .map_err(|e| win_err("WriteFile(device)", e))?;
        if written as usize != data.len() {
            return Err(BkError::Other(anyhow::anyhow!(
                "short device write: {written} of {}",
                data.len()
            )));
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        // WRITE_THROUGH already bypasses the cache; nothing buffered to flush.
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for DeviceTarget {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: handle from CreateFileW, closed once.
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }
}

// ---------------------------------------------------------------------------
// Non-Windows placeholder so the whole crate (and the portable `backup`
// pipeline) still compiles on Linux. The eventual Linux backend opens the
// block device with `O_DIRECT`, gets the size via `ioctl(BLKGETSIZE64)`, and
// reads sector-aligned windows into an [`AlignedBuf`] — the exact same public
// API as the Windows impl, so `backup.rs` needs no changes.
// ---------------------------------------------------------------------------
#[cfg(not(windows))]
pub struct SectorReader {
    len: u64,
}

#[cfg(not(windows))]
impl SectorReader {
    pub fn open(_device_path: &str) -> Result<Self> {
        Err(BkError::Other(anyhow::anyhow!(
            "SectorReader is not yet implemented on this platform (Linux O_DIRECT backend pending)"
        )))
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn read_window(&mut self, _buf: &mut [u8]) -> Result<usize> {
        unreachable!("open() errors on non-Windows platforms")
    }
}

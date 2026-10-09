//! bkcore — the backup engine.
//!
//! Nothing in here knows about a GUI or a CLI; both front-ends drive these
//! modules. The v1 pipeline is:
//!
//! ```text
//!   vss::snapshot(volume)          -> a frozen, consistent view of the live volume
//!     -> diskio::SectorReader       -> aligned, unbuffered reads of that view
//!       -> chunk::Chunker           -> split into blocks, BLAKE3 hash each
//!         -> manifest diff          -> keep only blocks whose hash is new/changed
//!           -> store::ChunkStore    -> content-addressed write (dedup for free)
//!             -> verify pass        -> re-read + re-hash, prove restorability
//! ```
//!
//! v2 adds [`usn`]: instead of reading the whole volume, ask the NTFS change
//! journal what changed since the last backup and only re-hash those files.

pub mod apply;
pub mod backup;
pub mod chunk;
pub mod diskio;
pub mod file;
pub mod job;
pub mod manifest;
pub mod mounts;
pub mod opt;
pub mod paths;
pub mod scheduler;
pub mod settings;
pub mod store;
pub mod timefmt;
pub mod update;
pub mod usn;
pub mod vss;

/// The product version, shared by every crate in the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Bytes per chunk. 1 MiB is a good default: big enough that hashing overhead
/// is negligible, small enough that a localized change re-transfers little.
/// Fixed-size for v1; we can move to content-defined chunking later if we see
/// poor dedup on shifted data.
pub const CHUNK_SIZE: usize = 1024 * 1024;

/// Alignment required for unbuffered (`FILE_FLAG_NO_BUFFERING`) disk I/O.
/// Must be a multiple of the volume's physical sector size; 4096 covers every
/// modern disk (512e and 4Kn alike).
pub const SECTOR_ALIGN: usize = 4096;

/// How much we read from the source per I/O. A multiple of both `CHUNK_SIZE`
/// (so windows never split a chunk) and `SECTOR_ALIGN` (so unbuffered reads are
/// legal). 8 MiB balances throughput against memory.
pub const READ_WINDOW: usize = 8 * CHUNK_SIZE;

#[derive(Debug, thiserror::Error)]
pub enum BkError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("this operation requires Administrator/elevation")]
    NotElevated,
    #[error("volume shadow copy failed: {0}")]
    Vss(String),
    #[error("backup store is corrupt or incomplete: {0}")]
    StoreCorrupt(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, BkError>;

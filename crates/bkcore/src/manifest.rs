//! A manifest is the authoritative description of one backup: the ordered list
//! of chunks that reconstruct the source volume, plus provenance.
//!
//! The manifest is the *commit point*. Chunks get streamed into the store
//! first; only once every chunk is durably written do we write the manifest
//! atomically. A backup without a manifest is invisible and ignored — this is
//! how an interrupted run leaves no half-valid backup behind.

use crate::chunk::ChunkRef;
use serde::{Deserialize, Serialize};

/// The current manifest on-disk format version. Bump on breaking changes.
pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// Human label of the job that produced this (e.g. "SystemDrive").
    pub job: String,
    /// Volume this backup represents, e.g. `\\?\Volume{GUID}\` or `C:`.
    pub source_volume: String,
    /// Total size of the source volume in bytes.
    pub volume_size: u64,
    /// Unix seconds the backup completed. Filled in by the caller (bkcore
    /// avoids reading the clock so the engine stays deterministic/testable).
    pub created_unix: u64,
    /// The manifest this one was built as a delta against, if any.
    /// `None` means a full backup.
    pub parent: Option<String>,
    /// USN journal position captured at snapshot time, so the *next* backup
    /// can ask "what changed since here?". `None` until v2 wiring lands.
    pub usn_high_water: Option<u64>,
    /// Every chunk needed to reconstruct the volume, in offset order.
    pub chunks: Vec<ManifestChunk>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestChunk {
    pub offset: u64,
    pub len: u32,
    /// Hex BLAKE3, matching the store's key for this chunk.
    pub hash: String,
}

impl Manifest {
    pub fn to_entry(r: &ChunkRef) -> ManifestChunk {
        ManifestChunk {
            offset: r.offset,
            len: r.len,
            hash: hex(&r.hash),
        }
    }

    /// The set of chunk hashes already present in this (parent) manifest.
    /// A new backup only stores chunks whose hash isn't in here — that's the
    /// delta.
    pub fn hash_set(&self) -> std::collections::HashSet<&str> {
        self.chunks.iter().map(|c| c.hash.as_str()).collect()
    }
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Parse a 64-char hex string back into a 32-byte BLAKE3 digest. Returns `None`
/// on wrong length or non-hex input.
pub fn unhex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

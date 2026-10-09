//! Applying a backup to a target — the shared engine behind **image restore**
//! and **drive cloning**. Both reconstruct a volume from a manifest + chunk
//! store; they differ only in the [`BlockTarget`] they write into:
//!
//!   - restore -> [`FileTarget`](crate::diskio::FileTarget) (a `.img`)
//!   - clone   -> [`DeviceTarget`](crate::diskio::DeviceTarget) (`\\.\PhysicalDriveN`)
//!
//! Incremental clone: when `delta` is set, each chunk is compared against what's
//! already on the target (read + hash) and skipped if identical. So re-cloning
//! onto a disk that already holds a previous clone writes only the changed
//! blocks — the fast, low-wear continuous-clone behavior.
//!
//! Portable: no platform types here.

use crate::chunk::hash_chunk;
use crate::diskio::BlockTarget;
use crate::manifest::{unhex, Manifest};
use crate::store::ChunkStore;
use crate::{BkError, Result};

#[derive(Debug, Clone)]
pub struct ApplySummary {
    /// Chunks actually written to the target.
    pub chunks_written: usize,
    /// Chunks skipped because the target already held identical bytes (delta).
    pub chunks_skipped: usize,
    pub bytes_written: u64,
    pub verified: bool,
}

/// Reconstruct `manifest` into `target`.
///
/// * `delta` — if true, skip chunks already identical on the target (incremental
///   clone). If false, write every chunk (fresh restore/clone).
/// * `verify_after` — re-read every chunk back from the target and confirm its
///   hash matches the manifest, proving the restore/clone is faithful.
pub fn apply(
    manifest: &Manifest,
    store: &ChunkStore,
    target: &mut dyn BlockTarget,
    delta: bool,
    verify_after: bool,
) -> Result<ApplySummary> {
    if target.len() < manifest.volume_size {
        return Err(BkError::Other(anyhow::anyhow!(
            "target is {} bytes but the backup needs {} — target too small",
            target.len(),
            manifest.volume_size
        )));
    }

    let mut chunks_written = 0usize;
    let mut chunks_skipped = 0usize;
    let mut bytes_written = 0u64;
    let mut cmpbuf: Vec<u8> = Vec::new();

    for c in &manifest.chunks {
        let expected = unhex(&c.hash)
            .ok_or_else(|| BkError::StoreCorrupt(format!("bad hash hex at offset {}", c.offset)))?;

        if delta {
            // Compare what's already there; skip the write if it matches.
            cmpbuf.resize(c.len as usize, 0);
            let n = target.read_at(c.offset, &mut cmpbuf)?;
            if n == c.len as usize && hash_chunk(&cmpbuf) == expected {
                chunks_skipped += 1;
                continue;
            }
        }

        let bytes = store.get(&expected)?;
        target.write_at(c.offset, &bytes)?;
        chunks_written += 1;
        bytes_written += bytes.len() as u64;
    }

    target.flush()?;

    let verified = if verify_after {
        verify_target(manifest, target)?;
        true
    } else {
        false
    };

    Ok(ApplySummary {
        chunks_written,
        chunks_skipped,
        bytes_written,
        verified,
    })
}

/// Read every chunk back from the target and confirm it hashes to what the
/// manifest recorded — proof the restore/clone is byte-faithful.
pub fn verify_target(manifest: &Manifest, target: &mut dyn BlockTarget) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    for c in &manifest.chunks {
        let expected = unhex(&c.hash)
            .ok_or_else(|| BkError::StoreCorrupt(format!("bad hash hex at offset {}", c.offset)))?;
        buf.resize(c.len as usize, 0);
        let n = target.read_at(c.offset, &mut buf)?;
        if n != c.len as usize {
            return Err(BkError::StoreCorrupt(format!(
                "target short read at offset {}: {n} != {}",
                c.offset, c.len
            )));
        }
        if hash_chunk(&buf) != expected {
            return Err(BkError::StoreCorrupt(format!(
                "target mismatch at offset {} — restore/clone is not faithful",
                c.offset
            )));
        }
    }
    Ok(())
}

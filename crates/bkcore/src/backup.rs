//! The v1 backup pipeline — portable, platform-agnostic glue over the pieces:
//!
//! ```text
//!   SectorReader (source)  ->  chunk + BLAKE3 hash  ->  is the hash already
//!   in the store?  --yes-->  skip (this is the delta: unchanged blocks cost
//!   nothing)  --no-->  store.put(chunk)  ->  record in manifest  ->
//!   commit_manifest (the atomic "this backup is now valid" point)  ->
//!   optional verify (re-read + re-hash every chunk).
//! ```
//!
//! The delta falls out of content addressing for free: a chunk whose bytes are
//! unchanged hashes to the same key, which already exists in the store, so a
//! scheduled backup only *writes* the blocks that actually changed — no kernel
//! CBT driver required. `parent` records the prior backup so the manifest chain
//! is explicit.
//!
//! Nothing here reads the clock (`created_unix` is passed in) so a backup run is
//! deterministic and unit-testable.

use crate::chunk::chunk_slice;
use crate::diskio::{AlignedBuf, SectorReader};
use crate::manifest::{unhex, Manifest, ManifestChunk, MANIFEST_VERSION};
use crate::store::ChunkStore;
use crate::{BkError, Result, CHUNK_SIZE, READ_WINDOW};
use std::path::{Path, PathBuf};

/// Everything one backup pass needs. Borrowed so callers don't have to give up
/// ownership of their job config.
pub struct BackupParams<'a> {
    /// Stable job id; used in manifest + task names.
    pub job_name: &'a str,
    /// Path handed to [`SectorReader::open`] (a device, VSS shadow, or file).
    pub source_device: &'a str,
    /// What to record as the source in the manifest (e.g. "C:").
    pub source_label: &'a str,
    /// Root of the chunk store.
    pub store_root: &'a Path,
    /// Unix seconds to stamp the manifest with (caller reads the clock).
    pub created_unix: u64,
    /// Re-read + re-hash every stored chunk after commit to prove restorability.
    pub verify_after: bool,
}

/// What a completed backup did — surfaced to the CLI/GUI so the delta is
/// visible (new vs deduped is the whole point).
#[derive(Debug, Clone)]
pub struct BackupSummary {
    pub manifest_path: PathBuf,
    pub manifest_stem: String,
    pub total_chunks: usize,
    /// Chunks whose bytes were new and got written this run.
    pub new_chunks: usize,
    /// Chunks already present in the store (unchanged) — the delta savings.
    pub deduped_chunks: usize,
    pub bytes_read: u64,
    pub parent: Option<String>,
    pub verified: bool,
}

/// The stem (filename without extension) a manifest is stored under.
fn manifest_stem(job: &str, created_unix: u64) -> String {
    format!("{job}-{created_unix}")
}

/// Run one backup pass.
pub fn run(params: &BackupParams) -> Result<BackupSummary> {
    let store = ChunkStore::open(params.store_root)?;

    // The previous backup for this job (if any) becomes our parent. Dedup is
    // decided against the *store*, not just this parent, so a chunk that
    // reappears from any past backup is still free.
    let parent = store.latest_manifest(params.job_name)?;
    let parent_stem = parent
        .as_ref()
        .map(|m| manifest_stem(&m.job, m.created_unix));

    let mut reader = SectorReader::open(params.source_device)?;
    let volume_size = reader.len();

    let mut buf = AlignedBuf::new(READ_WINDOW);
    let mut manifest_chunks: Vec<ManifestChunk> = Vec::new();
    let mut new_chunks = 0usize;
    let mut deduped_chunks = 0usize;
    let mut offset = 0u64;

    loop {
        let n = reader.read_window(buf.as_mut_slice())?;
        if n == 0 {
            break; // EOF
        }
        let window = &buf.as_slice()[..n];

        for cref in chunk_slice(offset, window) {
            let entry = Manifest::to_entry(&cref);
            if store.has(&cref.hash) {
                // Unchanged / already stored — the delta win.
                deduped_chunks += 1;
            } else {
                let start = (cref.offset - offset) as usize;
                let bytes = &window[start..start + cref.len as usize];
                store.put(&cref, bytes)?;
                new_chunks += 1;
            }
            manifest_chunks.push(entry);
        }
        offset += n as u64;
    }

    let total_chunks = manifest_chunks.len();
    let manifest = Manifest {
        version: MANIFEST_VERSION,
        job: params.job_name.to_string(),
        source_volume: params.source_label.to_string(),
        volume_size,
        created_unix: params.created_unix,
        parent: parent_stem.clone(),
        usn_high_water: None, // v2
        chunks: manifest_chunks,
    };

    let stem = manifest_stem(params.job_name, params.created_unix);
    // Commit is the atomic point: only now is the backup "real". Every chunk it
    // references was durably `put` above.
    let manifest_path = store.commit_manifest(&stem, &manifest)?;

    let verified = if params.verify_after {
        verify(&store, &manifest)?;
        true
    } else {
        false
    };

    Ok(BackupSummary {
        manifest_path,
        manifest_stem: stem,
        total_chunks,
        new_chunks,
        deduped_chunks,
        bytes_read: offset,
        parent: parent_stem,
        verified,
    })
}

/// Prove a manifest is restorable: read every chunk back from the store,
/// decompress it, re-hash, and confirm it matches what the manifest claims.
/// Also checks the chunk lengths sum to the recorded volume size. Errors on the
/// first discrepancy — a backup you can't verify is not a backup.
pub fn verify(store: &ChunkStore, manifest: &Manifest) -> Result<()> {
    let mut total_len: u64 = 0;
    for (i, c) in manifest.chunks.iter().enumerate() {
        let expected = unhex(&c.hash)
            .ok_or_else(|| BkError::StoreCorrupt(format!("chunk {i}: bad hash hex")))?;
        let bytes = store.get(&expected)?;
        if bytes.len() as u32 != c.len {
            return Err(BkError::StoreCorrupt(format!(
                "chunk {i} at offset {}: length {} != manifest {}",
                c.offset,
                bytes.len(),
                c.len
            )));
        }
        let actual = crate::chunk::hash_chunk(&bytes);
        if actual != expected {
            return Err(BkError::StoreCorrupt(format!(
                "chunk {i} at offset {}: hash mismatch (store corrupt)",
                c.offset
            )));
        }
        total_len += c.len as u64;
    }
    // Chunks should tile the volume with only the final one possibly short.
    if total_len != manifest.volume_size {
        return Err(BkError::StoreCorrupt(format!(
            "chunk lengths sum to {total_len} but volume size is {}",
            manifest.volume_size
        )));
    }
    let _ = CHUNK_SIZE; // (kept for future stricter offset/tiling checks)
    Ok(())
}

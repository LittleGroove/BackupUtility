//! The chunk store: a content-addressed, deduplicating repository on disk.
//!
//! Layout under the store root:
//! ```text
//!   <root>/
//!     chunks/<aa>/<aabbcc..>.zst      one file per unique chunk, key = BLAKE3 hex
//!     manifests/<job>-<created>.json  one manifest per completed backup
//!     tmp/                            in-flight writes, fsync'd then renamed
//! ```
//!
//! Two safety rules make this crash-consistent:
//!   1. A chunk is written to `tmp/`, flushed, then atomically renamed into
//!      `chunks/`. A reader never sees a partial chunk.
//!   2. A manifest is written the same way and only after every one of its
//!      chunks exists. So an interrupted backup leaves orphan chunks (harmless,
//!      reclaimable by GC) but never a manifest that points at missing data.

use crate::chunk::{ChunkHash, ChunkRef};
use crate::manifest::{hex, Manifest};
use crate::{BkError, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct ChunkStore {
    root: PathBuf,
}

impl ChunkStore {
    /// Open (creating if needed) a store rooted at `root`.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        for sub in ["chunks", "manifests", "tmp"] {
            fs::create_dir_all(root.join(sub))?;
        }
        Ok(Self { root })
    }

    fn chunk_path(&self, hash: &ChunkHash) -> PathBuf {
        let h = hex(hash);
        // Fan out by first byte so no single dir holds a million files.
        self.root
            .join("chunks")
            .join(&h[0..2])
            .join(format!("{h}.zst"))
    }

    /// True if this chunk is already stored (the fast path for unchanged data).
    pub fn has(&self, hash: &ChunkHash) -> bool {
        self.chunk_path(hash).exists()
    }

    /// Store one chunk's bytes if absent. Returns `true` if it was newly
    /// written, `false` if it already existed (deduped).
    pub fn put(&self, r: &ChunkRef, bytes: &[u8]) -> Result<bool> {
        let dst = self.chunk_path(&r.hash);
        if dst.exists() {
            return Ok(false);
        }
        fs::create_dir_all(dst.parent().unwrap())?;

        // Compress into a temp file, flush, then atomic rename.
        let tmp = self.root.join("tmp").join(format!("{}.part", hex(&r.hash)));
        {
            let f = fs::File::create(&tmp)?;
            let mut enc = zstd::Encoder::new(f, 3)?;
            enc.write_all(bytes)?;
            let f = enc.finish()?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &dst)?;
        Ok(true)
    }

    /// Read and decompress a chunk by hash. Errors if missing/corrupt — a
    /// missing chunk means the store lied about a manifest, which we treat as
    /// corruption, not a soft miss.
    pub fn get(&self, hash: &ChunkHash) -> Result<Vec<u8>> {
        let path = self.chunk_path(hash);
        let f = fs::File::open(&path)
            .map_err(|_| BkError::StoreCorrupt(format!("chunk {} missing", hex(hash))))?;
        let mut out = Vec::new();
        zstd::Decoder::new(f)?.read_to_end(&mut out)?;
        Ok(out)
    }

    /// Atomically commit a completed manifest. Call only after every chunk it
    /// references is durably `put`. `file_stem` should be unique per backup.
    pub fn commit_manifest(&self, file_stem: &str, m: &Manifest) -> Result<PathBuf> {
        let dst = self
            .root
            .join("manifests")
            .join(format!("{file_stem}.json"));
        let tmp = self.root.join("tmp").join(format!("{file_stem}.json.part"));
        let json = serde_json::to_vec_pretty(m).map_err(|e| BkError::Other(e.into()))?;
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &dst)?;
        Ok(dst)
    }

    /// Load the most recent manifest for a job, if any — the parent for the
    /// next delta backup.
    pub fn latest_manifest(&self, job: &str) -> Result<Option<Manifest>> {
        let mut all = self.list_manifests(job)?;
        // list_manifests sorts oldest-first; the parent is the newest.
        Ok(all.pop())
    }

    /// Load one manifest by its file stem (e.g. "job-1784703000").
    pub fn load_manifest(&self, stem: &str) -> Result<Manifest> {
        let path = self.root.join("manifests").join(format!("{stem}.json"));
        let data = fs::read(&path)?;
        serde_json::from_slice(&data).map_err(|e| BkError::Other(e.into()))
    }

    /// All manifests for a job, sorted oldest-first by creation time.
    pub fn list_manifests(&self, job: &str) -> Result<Vec<Manifest>> {
        let dir = self.root.join("manifests");
        let mut found: Vec<Manifest> = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let data = fs::read(entry.path())?;
            let m: Manifest = match serde_json::from_slice(&data) {
                Ok(m) => m,
                Err(_) => continue, // skip anything unparseable
            };
            if m.job == job {
                found.push(m);
            }
        }
        found.sort_by_key(|m| m.created_unix);
        Ok(found)
    }
}

// Bring `Read::read_to_end` into scope for the decoder above.
use std::io::Read;

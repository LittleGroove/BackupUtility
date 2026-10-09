//! Splitting a byte stream into fixed-size chunks and hashing each with BLAKE3.
//!
//! BLAKE3 is the right hash here: it's faster than memcpy on modern CPUs
//! (SIMD + internal parallelism), cryptographically strong, and 32 bytes is
//! plenty to make a collision astronomically unlikely across a drive's worth
//! of chunks. The hash *is* the chunk's identity in the store (content
//! addressing), which is what buys us dedup and the delta comparison.

use crate::CHUNK_SIZE;

/// A 256-bit BLAKE3 digest, used as a chunk's content address.
pub type ChunkHash = [u8; 32];

/// One chunk's position and identity within a source volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRef {
    /// Byte offset of this chunk from the start of the source volume.
    pub offset: u64,
    /// Actual byte length (the final chunk may be shorter than `CHUNK_SIZE`).
    pub len: u32,
    /// BLAKE3 of the chunk's bytes.
    pub hash: ChunkHash,
}

/// Hash a single chunk's bytes.
pub fn hash_chunk(bytes: &[u8]) -> ChunkHash {
    *blake3::hash(bytes).as_bytes()
}

/// Walk `data` in `CHUNK_SIZE` steps, yielding a [`ChunkRef`] per block.
/// `base_offset` is where `data` begins within the source volume, so refs
/// carry absolute offsets even when we feed the reader in windows.
pub fn chunk_slice(base_offset: u64, data: &[u8]) -> Vec<ChunkRef> {
    data.chunks(CHUNK_SIZE)
        .enumerate()
        .map(|(i, c)| ChunkRef {
            offset: base_offset + (i * CHUNK_SIZE) as u64,
            len: c.len() as u32,
            hash: hash_chunk(c),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_and_content_addressed() {
        let a = vec![7u8; CHUNK_SIZE + 123];
        let refs = chunk_slice(0, &a);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].offset, 0);
        assert_eq!(refs[1].offset, CHUNK_SIZE as u64);
        assert_eq!(refs[1].len, 123);
        // Identical bytes -> identical hash (the basis for dedup + delta).
        assert_eq!(refs[0].hash, hash_chunk(&vec![7u8; CHUNK_SIZE]));
    }
}

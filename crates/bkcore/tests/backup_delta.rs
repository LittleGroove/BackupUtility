//! End-to-end proof of the delta engine, run against a plain file (no admin,
//! no VSS). Backs up a source, changes exactly one 1 MiB block, backs up again,
//! and asserts the second pass stores only the changed block.
//!
//! Windows-only for now: it exercises the real `SectorReader` (unbuffered I/O),
//! which is the platform-specific piece. The pipeline it drives is portable.

#![cfg(windows)]

use bkcore::backup::{self, BackupParams};
use bkcore::store::ChunkStore;
use bkcore::{CHUNK_SIZE, SECTOR_ALIGN};
use std::fs;
use std::path::PathBuf;

/// A unique-enough scratch dir without needing a clock/RNG (both banned in some
/// contexts). Process id + a caller-supplied tag keeps parallel tests apart.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bktest-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build a source image of `n_chunks` full chunks, each filled with a distinct
/// byte so every chunk hashes differently. Size is a multiple of SECTOR_ALIGN.
fn write_image(path: &PathBuf, n_chunks: usize) {
    assert!(CHUNK_SIZE % SECTOR_ALIGN == 0);
    let mut data = vec![0u8; n_chunks * CHUNK_SIZE];
    for i in 0..n_chunks {
        let fill = (i as u8).wrapping_add(1);
        data[i * CHUNK_SIZE..(i + 1) * CHUNK_SIZE].fill(fill);
    }
    fs::write(path, &data).unwrap();
}

#[test]
fn delta_backup_stores_only_changed_block() {
    let dir = scratch("delta");
    let src = dir.join("src.img");
    let store_root = dir.join("store");
    let n_chunks = 4;
    write_image(&src, n_chunks);

    let src_str = src.to_string_lossy().to_string();

    // --- First backup: everything is new. ---
    let first = backup::run(&BackupParams {
        job_name: "test",
        source_device: &src_str,
        source_label: "src.img",
        store_root: &store_root,
        created_unix: 1000,
        verify_after: true,
    })
    .expect("first backup");

    assert_eq!(first.total_chunks, n_chunks, "should see all chunks");
    assert_eq!(first.new_chunks, n_chunks, "first backup stores everything");
    assert_eq!(first.deduped_chunks, 0, "nothing to dedup on a full backup");
    assert_eq!(first.bytes_read, (n_chunks * CHUNK_SIZE) as u64);
    assert!(first.verified, "verify pass must pass");
    assert!(first.parent.is_none(), "first backup has no parent");

    // --- Change exactly one 1 MiB block (index 2). ---
    let mut data = fs::read(&src).unwrap();
    data[2 * CHUNK_SIZE..3 * CHUNK_SIZE].fill(0xAB);
    fs::write(&src, &data).unwrap();

    // --- Second backup: only the changed block is new. ---
    let second = backup::run(&BackupParams {
        job_name: "test",
        source_device: &src_str,
        source_label: "src.img",
        store_root: &store_root,
        created_unix: 2000,
        verify_after: true,
    })
    .expect("second backup");

    assert_eq!(second.total_chunks, n_chunks);
    assert_eq!(
        second.new_chunks, 1,
        "only the one changed block should be stored"
    );
    assert_eq!(
        second.deduped_chunks,
        n_chunks - 1,
        "the other three blocks are unchanged and deduped"
    );
    assert!(second.verified);
    assert_eq!(
        second.parent.as_deref(),
        Some("test-1000"),
        "second backup chains to the first"
    );

    // --- The store should hold n_chunks + 1 unique chunks: the original four
    //     plus the one new version of block 2 (old version is retained). ---
    let store = ChunkStore::open(&store_root).unwrap();
    let manifests = store.list_manifests("test").unwrap();
    assert_eq!(manifests.len(), 2, "two backups recorded");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn identical_reback_stores_nothing_new() {
    let dir = scratch("noop");
    let src = dir.join("src.img");
    let store_root = dir.join("store");
    write_image(&src, 3);
    let src_str = src.to_string_lossy().to_string();

    let params = |t: u64| BackupParams {
        job_name: "noop",
        source_device: &src_str,
        source_label: "src.img",
        store_root: &store_root,
        created_unix: t,
        verify_after: true,
    };

    let first = backup::run(&params(10)).unwrap();
    assert_eq!(first.new_chunks, 3);

    // Same bytes, second run: pure dedup, zero new chunks.
    let second = backup::run(&params(20)).unwrap();
    assert_eq!(second.new_chunks, 0, "unchanged source => no new storage");
    assert_eq!(second.deduped_chunks, 3);

    let _ = fs::remove_dir_all(&dir);
}

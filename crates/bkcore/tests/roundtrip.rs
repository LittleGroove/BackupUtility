//! Proves the apply/restore/clone core:
//!  1. back up a source, restore to a fresh image, assert byte-identical.
//!  2. incremental re-apply onto an existing image after one block changed —
//!     assert only that block is written, the rest skipped, result faithful.
//!
//! Uses FileTarget so it needs no admin (the DeviceTarget clone path shares the
//! exact same apply loop and is exercised under a VHD once elevated).

#![cfg(windows)]

use bkcore::apply::{self};
use bkcore::backup::{self, BackupParams};
use bkcore::diskio::FileTarget;
use bkcore::store::ChunkStore;
use bkcore::{CHUNK_SIZE, SECTOR_ALIGN};
use std::fs;
use std::path::PathBuf;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bkrt-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_image(path: &PathBuf, n_chunks: usize) {
    assert!(CHUNK_SIZE.is_multiple_of(SECTOR_ALIGN));
    let mut data = vec![0u8; n_chunks * CHUNK_SIZE];
    for i in 0..n_chunks {
        data[i * CHUNK_SIZE..(i + 1) * CHUNK_SIZE].fill((i as u8).wrapping_add(1));
    }
    fs::write(path, &data).unwrap();
}

#[test]
fn restore_is_byte_identical() {
    let dir = scratch("restore");
    let src = dir.join("src.img");
    let store_root = dir.join("store");
    let restored = dir.join("restored.img");
    let n = 5;
    write_image(&src, n);
    let src_str = src.to_string_lossy().to_string();

    let summary = backup::run(&BackupParams {
        job_name: "rt",
        source_device: &src_str,
        source_label: "src.img",
        store_root: &store_root,
        created_unix: 1,
        verify_after: true,
    })
    .unwrap();

    let store = ChunkStore::open(&store_root).unwrap();
    let manifest = store.latest_manifest("rt").unwrap().unwrap();

    // Fresh restore to a new image (delta = false).
    let mut target = FileTarget::create(&restored, manifest.volume_size).unwrap();
    let applied = apply::apply(&manifest, &store, &mut target, false, true).unwrap();
    assert_eq!(applied.chunks_written, n);
    assert_eq!(applied.chunks_skipped, 0);
    assert!(applied.verified);
    drop(target);

    // The real proof: bytes match exactly.
    assert_eq!(
        fs::read(&src).unwrap(),
        fs::read(&restored).unwrap(),
        "restored image must equal the source byte-for-byte"
    );
    let _ = summary;
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn incremental_clone_writes_only_changed_block() {
    let dir = scratch("incr");
    let src = dir.join("src.img");
    let store_root = dir.join("store");
    let target_img = dir.join("clone.img");
    let n = 6;
    write_image(&src, n);
    let src_str = src.to_string_lossy().to_string();

    let params = |t: u64| BackupParams {
        job_name: "cl",
        source_device: &src_str,
        source_label: "src.img",
        store_root: &store_root,
        created_unix: t,
        verify_after: true,
    };

    // First backup + full clone to the target image.
    backup::run(&params(1)).unwrap();
    let store = ChunkStore::open(&store_root).unwrap();
    let m1 = store.latest_manifest("cl").unwrap().unwrap();
    {
        let mut t = FileTarget::create(&target_img, m1.volume_size).unwrap();
        let a = apply::apply(&m1, &store, &mut t, false, true).unwrap();
        assert_eq!(a.chunks_written, n);
    }
    assert_eq!(fs::read(&src).unwrap(), fs::read(&target_img).unwrap());

    // Change exactly one block in the source, back it up again.
    let mut data = fs::read(&src).unwrap();
    data[3 * CHUNK_SIZE..4 * CHUNK_SIZE].fill(0x5A);
    fs::write(&src, &data).unwrap();
    backup::run(&params(2)).unwrap();
    let m2 = store.latest_manifest("cl").unwrap().unwrap();

    // Incremental re-clone onto the EXISTING target (delta = true).
    let mut t = FileTarget::open(&target_img).unwrap();
    let a = apply::apply(&m2, &store, &mut t, true, true).unwrap();
    assert_eq!(a.chunks_written, 1, "only the changed block is written");
    assert_eq!(a.chunks_skipped, n - 1, "the rest are already identical");
    assert!(a.verified);
    drop(t);

    // Target now matches the updated source exactly.
    assert_eq!(
        fs::read(&src).unwrap(),
        fs::read(&target_img).unwrap(),
        "incrementally-cloned target must equal the updated source"
    );
    let _ = fs::remove_dir_all(&dir);
}

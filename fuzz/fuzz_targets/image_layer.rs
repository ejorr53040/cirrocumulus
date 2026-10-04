//! An OCI image layer, as a registry serves it: whatever the bytes, merging
//! it must return an error or a tree, never panic, and the tree must stay
//! inside the rootfs (no absolute paths, no `..`).
#![no_main]

use libfuzzer_sys::fuzz_target;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static RUN: AtomicU64 = AtomicU64::new(0);

fuzz_target!(|data: &[u8]| {
    let dir = std::env::temp_dir().join(format!(
        "cirro-fuzz-{}-{}",
        std::process::id(),
        RUN.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let layer = dir.join("layer.tar");
    std::fs::write(&layer, data).unwrap();
    if let Ok(flat) = cirro_image::Flattened::open(&[layer], &dir.join("scratch")) {
        for path in flat.paths() {
            assert!(
                !path.starts_with('/') && !path.split('/').any(|part| part == ".."),
                "a layer put {path:?} outside the rootfs"
            );
        }
    }
    let _ = std::fs::remove_dir_all(PathBuf::from(&dir));
});

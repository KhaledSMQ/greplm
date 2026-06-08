//! Fuzz the index manifest decoder.
//!
//! `meta.json` can be truncated or corrupted by a crash mid-write. Loading it
//! must return a clean error (triggering rebuild), never panic.
#![no_main]

use greplm_core::meta::Meta;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = serde_json::from_slice::<Meta>(data);

    let mut path = std::env::temp_dir();
    path.push(format!(
        "greplm-fuzz-meta-{}-{}",
        std::process::id(),
        data.len()
    ));
    if std::fs::write(&path, data).is_ok() {
        let _ = Meta::load(&path);
        let _ = std::fs::remove_file(&path);
    }
});

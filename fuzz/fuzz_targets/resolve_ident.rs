//! Fuzz identifier resolution at arbitrary source positions.
//!
//! Go-to-definition resolves the identifier under a (line, column) in arbitrary
//! source bytes via tree-sitter. Malformed or non-UTF-8 input must not cause
//! out-of-bounds slicing or panics during the resolution walk.
#![no_main]

use greplm_core::lang::Language;
use greplm_core::resolve;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let lang = Language::ALL[data[0] as usize % Language::ALL.len()];

    let (line, col, source) = if data.len() >= 9 {
        let line = u32::from_le_bytes(data[1..5].try_into().unwrap());
        let col = u32::from_le_bytes(data[5..9].try_into().unwrap());
        (line, col, &data[9..])
    } else {
        (1, 1, &data[1..])
    };

    let _ = resolve::identifier_at(lang, source, line, col);
});

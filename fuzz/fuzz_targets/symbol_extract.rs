//! Fuzz symbol and reference extraction over arbitrary source bytes.
//!
//! Indexing parses every file with tree-sitter and walks the tree to pull out
//! symbol definitions and references, slicing the source by node byte offsets.
//! Malformed, truncated, or non-UTF-8 input (greplm indexes by bytes) must not
//! cause out-of-bounds slicing, integer-conversion panics, or other crashes
//! during this hot indexing path.
#![no_main]

use greplm_core::lang::Language;
use greplm_core::symbol;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // First byte selects the grammar; the rest is the source buffer.
    let lang = Language::ALL[data[0] as usize % Language::ALL.len()];
    let source = &data[1..];

    let _ = symbol::extract(lang, source);
    let _ = symbol::extract_all(lang, source);
});

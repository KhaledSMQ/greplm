//! Fuzz literal and regex match verification over arbitrary bytes.
//!
//! After trigram filtering, every content search runs the real literal/regex
//! matcher over file bytes (including whole-word boundary checks). Patterns are
//! attacker-controlled via the daemon/CLI, so this path must never panic.
#![no_main]

use greplm_core::search;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let flags = data[0];
    let regex = flags & 1 != 0;
    let case_insensitive = flags & 2 != 0;
    let whole_word = flags & 4 != 0;

    let body = &data[1..];
    let mid = body.len() / 2;
    let (pattern_bytes, hay) = body.split_at(mid);
    let Ok(pattern) = std::str::from_utf8(pattern_bytes) else {
        return;
    };

    search::fuzz_match_starts(pattern, hay, regex, case_insensitive, whole_word);
});

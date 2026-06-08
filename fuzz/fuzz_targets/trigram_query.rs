//! Fuzz trigram extraction and query decomposition.
//!
//! Every content query is turned into a [`TrigramQuery`] before it touches the
//! index: literals via `from_literal`/`from_literal_ci`, regexes via
//! `regex_trigrams`. These run on fully attacker-controlled patterns, so they
//! must never panic on arbitrary bytes — including invalid UTF-8, lone
//! surrogates' worth of bytes, and pathological regex syntax.
#![no_main]

use greplm_core::trigram::{self, TrigramQuery};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = trigram::extract(data);

    let lit = TrigramQuery::from_literal(data);
    let _ = lit.is_unconstrained();

    let lit_ci = TrigramQuery::from_literal_ci(data);
    let _ = lit_ci.is_unconstrained();

    // Regex extraction takes a &str; feed it whatever valid UTF-8 we have.
    if let Ok(pattern) = std::str::from_utf8(data) {
        let _ = trigram::regex_trigrams(pattern, false);
        let _ = trigram::regex_trigrams(pattern, true);
    }
});

//! Fuzz structural (AST) query compilation and execution.
//!
//! `structural::compile` accepts a user-supplied pattern (a raw tree-sitter
//! S-expression or greplm's `$META` meta-variable form) and lowers it to a
//! tree-sitter `Query`. Both the meta-variable rewriter and the grammar's query
//! parser run on arbitrary input; a bad pattern must return an `Err`, not
//! panic. Compiled queries are then run against a source buffer, which must
//! also be panic-free.
#![no_main]

use greplm_core::lang::Language;
use greplm_core::structural;
use libfuzzer_sys::fuzz_target;

const SAMPLE_SOURCE: &[u8] = b"fn helper(x) { call(x); }\nclass Foo { fn bar() {} }\n";

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // First byte selects the grammar; the rest is the pattern.
    let lang = Language::ALL[data[0] as usize % Language::ALL.len()];
    let Ok(pattern) = std::str::from_utf8(&data[1..]) else {
        return;
    };

    if let Ok(compiled) = structural::compile(lang, pattern) {
        let _ = structural::run(lang, &compiled, SAMPLE_SOURCE);
        // Also run against the pattern bytes themselves as a second source.
        let _ = structural::run(lang, &compiled, &data[1..]);
    }
});

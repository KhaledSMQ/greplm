//! Fuzz the daemon wire protocol decoder.
//!
//! The daemon reads one newline-delimited JSON object per request straight off
//! an untrusted socket and `serde_json`-decodes it into a [`Request`] /
//! [`RoutedRequest`]. A malformed or hostile line must produce a clean decode
//! error, never a panic, abort, or unbounded allocation that takes the server
//! down.
#![no_main]

use greplm_core::proto::{Request, RoutedRequest};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = serde_json::from_slice::<Request>(data);
    let _ = serde_json::from_slice::<RoutedRequest>(data);
});

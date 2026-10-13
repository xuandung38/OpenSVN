//! encode(v0/v1/v2) -> decode (chunked) -> apply must reproduce the input exactly.
#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use svn_delta::{SvndiffDecoder, SvndiffEncoder, SvndiffVersion, WindowApplier};

fuzz_target!(|input: &[u8]| {
    let Some((&sel, payload)) = input.split_first() else {
        return;
    };
    let version = match sel % 3 {
        0 => SvndiffVersion::V0,
        1 => SvndiffVersion::V1,
        _ => SvndiffVersion::V2,
    };
    let Ok(stream) = SvndiffEncoder::fulltext(version, payload) else {
        panic!("encoding valid input failed");
    };
    let chunk = usize::from(sel / 3).max(1);
    let mut decoder = SvndiffDecoder::new();
    let mut applier = WindowApplier::new(Cursor::new(Vec::new()), Vec::new());
    for part in stream.chunks(chunk) {
        let Ok(windows) = decoder.feed(part) else {
            panic!("decoding own output failed");
        };
        for w in &windows {
            if applier.apply_window(w).is_err() {
                panic!("applying own output failed");
            }
        }
    }
    if decoder.finish().is_err() || applier.finish().is_err() {
        panic!("finishing failed");
    }
    assert_eq!(applier.into_output(), payload);
});

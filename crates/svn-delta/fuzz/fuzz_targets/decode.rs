//! Arbitrary bytes must decode or fail cleanly, never panic, and applying whatever decodes must
//! do the same. Input is split at a data-driven point to exercise the chunk-resume paths.
#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use svn_delta::{SvndiffDecoder, WindowApplier};

fuzz_target!(|data: &[u8]| {
    let Some((&split, stream)) = data.split_first() else {
        return;
    };
    let cut = (usize::from(split) % (stream.len() + 1)).min(stream.len());
    let mut decoder = SvndiffDecoder::new();
    let mut applier = WindowApplier::new(Cursor::new(vec![0x5au8; 4096]), std::io::sink());
    for part in [&stream[..cut], &stream[cut..]] {
        match decoder.feed(part) {
            Ok(windows) => {
                for w in &windows {
                    let _ = applier.apply_window(w);
                }
            }
            Err(_) => return,
        }
    }
    let _ = decoder.finish();
});

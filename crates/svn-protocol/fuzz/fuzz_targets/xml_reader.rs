#![no_main]
//! The namespace-aware reader is the root of every parser: it must never panic or blow up.
use libfuzzer_sys::fuzz_target;
use svn_protocol::xml::XmlReader;

fuzz_target!(|data: &[u8]| {
    let mut r = XmlReader::new();
    for chunk in data.chunks(7) {
        if r.feed(chunk).is_err() {
            return;
        }
    }
    let _ = r.finish();
});

#![no_main]
//! PROPFIND parsing must never panic, whatever the bytes and however they are chunked.
use libfuzzer_sys::fuzz_target;
use svn_protocol::ResponseParser;
use svn_protocol::propfind::MultiStatusParser;

fuzz_target!(|data: &[u8]| {
    let split = data.first().map_or(0, |b| usize::from(*b)) % (data.len() + 1);
    let mut p = MultiStatusParser::new();
    let _ = p.feed(&data[..split]);
    let _ = p.feed(&data[split..]);
    let _ = p.finish();
});

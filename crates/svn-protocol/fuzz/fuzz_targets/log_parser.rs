#![no_main]
//! log-report parsing must never panic, whatever the bytes and however they are chunked.
use libfuzzer_sys::fuzz_target;
use svn_protocol::ResponseParser;
use svn_protocol::log::LogParser;

fuzz_target!(|data: &[u8]| {
    let split = data.first().map_or(0, |b| usize::from(*b)) % (data.len() + 1);
    let mut p = LogParser::new();
    let _ = p.feed(&data[..split]);
    let _ = p.feed(&data[split..]);
    let _ = p.finish();
});

#![no_main]
//! get-locks-report parsing must never panic.
use libfuzzer_sys::fuzz_target;
use svn_protocol::ResponseParser;
use svn_protocol::locks::GetLocksParser;

fuzz_target!(|data: &[u8]| {
    let split = data.first().map_or(0, |b| usize::from(*b)) % (data.len() + 1);
    let mut p = GetLocksParser::new();
    let _ = p.feed(&data[..split]);
    let _ = p.feed(&data[split..]);
    let _ = p.finish();
});

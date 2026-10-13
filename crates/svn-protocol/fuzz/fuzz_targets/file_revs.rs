#![no_main]
//! file-revs-report and merge-response parsing must never panic.
use libfuzzer_sys::fuzz_target;
use svn_protocol::ResponseParser;
use svn_protocol::commit::MergeResponseParser;
use svn_protocol::file_revs::FileRevsParser;

fuzz_target!(|data: &[u8]| {
    let split = data.first().map_or(0, |b| usize::from(*b)) % (data.len() + 1);
    let mut p = FileRevsParser::new();
    let _ = p.feed(&data[..split]);
    let _ = p.feed(&data[split..]);
    let _ = p.finish();
    let mut m = MergeResponseParser::new();
    let _ = m.feed(&data[..split]);
    let _ = m.feed(&data[split..]);
    let _ = m.finish();
});

//! Cheap stand-in for a fuzz run on every `cargo test`: corrupt recorded responses in random ways
//! and require that every parser returns instead of panicking or hanging.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{load, urls};
use proptest::prelude::*;
use svn_protocol::ResponseParser;
use svn_protocol::locks::GetLocksParser;
use svn_protocol::log::LogParser;
use svn_protocol::propfind::{ListParser, MultiStatusParser};
use svn_protocol::{Headers, error_from_response};
use svn_types::RelPath;

const CORPUS: [&str; 6] = [
    "log/head-verbose",
    "log/merged-revisions",
    "propfind/list-depth1",
    "propfind/allprop-file",
    "locks/get-locks-report",
    "errors/out-of-date-409",
];

fn drive<P: ResponseParser>(mut p: P, data: &[u8], split: usize) {
    let split = split.min(data.len());
    let _ = p.feed(&data[..split]);
    let _ = p.feed(&data[split..]);
    let _ = p.finish();
}

fn run_all(data: &[u8], split: usize) {
    let u = urls();
    drive(LogParser::new(), data, split);
    drive(MultiStatusParser::new(), data, split);
    drive(GetLocksParser::new(), data, split);
    drive(
        ListParser::new(&u, RelPath::new("trunk").unwrap()),
        data,
        split,
    );
    let _ = error_from_response(409, &Headers::new(), data);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn flipped_bytes(idx in 0usize..6, flips in proptest::collection::vec((0usize..65536, any::<u8>()), 1..8), split in 0usize..65536) {
        let mut data = load(CORPUS[idx]).resp_body;
        for (pos, byte) in flips {
            let len = data.len();
            data[pos % len] = byte;
        }
        run_all(&data, split);
    }

    #[test]
    fn truncated(idx in 0usize..6, cut in 0usize..65536, split in 0usize..65536) {
        let data = load(CORPUS[idx]).resp_body;
        run_all(&data[..cut % (data.len() + 1)], split);
    }

    #[test]
    fn spliced(idx in 0usize..6, other in 0usize..6, at in 0usize..65536, from in 0usize..65536, split in 0usize..65536) {
        let a = load(CORPUS[idx]).resp_body;
        let b = load(CORPUS[other]).resp_body;
        let mut data = a[..at % (a.len() + 1)].to_vec();
        data.extend_from_slice(&b[from % (b.len() + 1)..]);
        run_all(&data, split);
    }
}

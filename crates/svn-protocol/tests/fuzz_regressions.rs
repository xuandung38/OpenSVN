//! Inputs that crashed a fuzz target, replayed through the same entry points on stable Rust.
//!
//! Each file in `tests/data/fuzz-regressions/<target>/` is a libFuzzer artifact of that target
//! (`fuzz/fuzz_targets/<target>.rs`); the replay below mirrors the target body. A new crash goes
//! into the matching directory together with the parser fix.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use svn_protocol::ResponseParser;
use svn_protocol::commit::MergeResponseParser;
use svn_protocol::file_revs::FileRevsParser;
use svn_protocol::list_report::ListReportParser;

fn inputs(target: &str) -> Vec<(String, Vec<u8>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/fuzz-regressions")
        .join(target);
    let mut out: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| {
            let path = e.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read(&path).unwrap())
        })
        .collect();
    out.sort();
    assert!(!out.is_empty(), "no inputs in {}", dir.display());
    out
}

/// Same body as `fuzz/fuzz_targets/file_revs.rs`.
fn file_revs(data: &[u8]) {
    let split = data.first().map_or(0, |b| usize::from(*b)) % (data.len() + 1);
    let mut p = FileRevsParser::new();
    let _ = p.feed(&data[..split]);
    let _ = p.feed(&data[split..]);
    let _ = p.finish();
    let mut l = ListReportParser::new(svn_types::RelPath::root());
    let _ = l.feed(&data[..split]);
    let _ = l.feed(&data[split..]);
    let _ = l.finish();
    let mut m = MergeResponseParser::new();
    let _ = m.feed(&data[..split]);
    let _ = m.feed(&data[split..]);
    let _ = m.finish();
}

#[test]
fn file_revs_crashes_stay_fixed() {
    for (name, data) in inputs("file_revs") {
        eprintln!("replaying {name}");
        file_revs(&data);
    }
}

/// The crash itself: a non-ASCII character inside `S:txdelta` base64 is a corrupt response, not
/// a panic, whatever the chunking.
#[test]
fn non_ascii_in_txdelta_base64_is_reported_as_corrupt() {
    let (_, data) = inputs("file_revs").remove(0);
    for split in 0..=data.len() {
        let mut p = FileRevsParser::new();
        let first = p.feed(&data[..split]);
        let second = p.feed(&data[split..]);
        let end = p.finish();
        assert!(
            first.is_err() || second.is_err() || end.is_err(),
            "split {split}: the corrupt response was accepted"
        );
    }
}

//! Decodes and applies real svndiff streams written by `svnadmin` (svn 1.15 FSFS) for all three
//! versions. See `tests/data/generate.py` for how the fixtures were produced.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Cursor;
use std::path::PathBuf;

use md5::{Digest, Md5};
use svn_delta::{Base64SvndiffDecoder, SvndiffDecoder, WindowApplier};
use svn_types::WindowSink;

struct Case {
    name: String,
    svndiff: Vec<u8>,
    base: Vec<u8>,
    expected: Vec<u8>,
    md5: String,
}

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

fn cases() -> Vec<Case> {
    let dir = data_dir();
    let manifest = std::fs::read_to_string(dir.join("manifest.txt")).expect("manifest");
    manifest
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split(' ').collect();
            let read = |n: &str| std::fs::read(dir.join(n)).expect("fixture");
            Case {
                name: f[0].to_owned(),
                svndiff: read(f[0]),
                base: if f[1] == "-" { Vec::new() } else { read(f[1]) },
                expected: read(f[2]),
                md5: f[3].to_owned(),
            }
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn apply_with_chunks(c: &Case, chunk: usize) -> Vec<u8> {
    let mut dec = SvndiffDecoder::new();
    let mut app = WindowApplier::new(Cursor::new(c.base.clone()), Vec::new());
    for piece in c.svndiff.chunks(chunk) {
        dec.feed_sink(piece, &mut app).expect("decode");
    }
    dec.finish_sink(&mut app).expect("finish");
    app.into_output()
}

#[test]
fn every_fixture_reconstructs_the_exact_text() {
    let all = cases();
    assert_eq!(all.len(), 9, "3 versions x 3 revisions");
    for c in &all {
        let out = apply_with_chunks(c, usize::MAX);
        assert_eq!(out, c.expected, "{}", c.name);
        assert_eq!(hex(&Md5::digest(&out)), c.md5, "{}", c.name);
    }
}

#[test]
fn chunk_boundaries_do_not_matter() {
    for c in &cases() {
        for chunk in [1, 2, 3, 7, 64, 4093] {
            assert_eq!(
                apply_with_chunks(c, chunk),
                c.expected,
                "{} chunk {chunk}",
                c.name
            );
        }
    }
}

#[test]
fn compressed_versions_really_are_compressed() {
    let all = cases();
    let size = |n: &str| all.iter().find(|c| c.name == n).unwrap().svndiff.len();
    assert!(size("zlib-r1.svndiff") < size("none-r1.svndiff") / 3);
    assert!(size("lz4-r1.svndiff") < size("none-r1.svndiff"));
    let first = |n: &str| all.iter().find(|c| c.name == n).unwrap().svndiff[3];
    assert_eq!(
        [
            first("none-r1.svndiff"),
            first("zlib-r1.svndiff"),
            first("lz4-r1.svndiff")
        ],
        [0, 1, 2]
    );
}

#[test]
fn large_fixture_spans_multiple_windows() {
    let c = cases()
        .into_iter()
        .find(|c| c.name == "zlib-r1.svndiff")
        .unwrap();
    let mut dec = SvndiffDecoder::new();
    let windows = dec.feed(&c.svndiff).unwrap();
    dec.finish().unwrap();
    assert!(windows.len() >= 2, "got {}", windows.len());
}

#[test]
fn deltas_use_source_copies() {
    let c = cases()
        .into_iter()
        .find(|c| c.name == "lz4-r2.svndiff")
        .unwrap();
    let mut dec = SvndiffDecoder::new();
    let windows = dec.feed(&c.svndiff).unwrap();
    assert!(
        windows
            .iter()
            .flat_map(|w| &w.ops)
            .any(|op| matches!(op, svn_types::TxDeltaOp::CopyFromSource { .. }))
    );
}

#[test]
fn base64_wrapped_streams_decode_with_arbitrary_splits() {
    use base64::Engine;
    for c in &cases() {
        let text = base64::engine::general_purpose::STANDARD.encode(&c.svndiff);
        // XML producers wrap lines.
        let wrapped: String = text
            .as_bytes()
            .chunks(76)
            .map(|l| String::from_utf8(l.to_vec()).unwrap() + "\n")
            .collect();
        for split in [1, 5, 4096] {
            let mut dec = Base64SvndiffDecoder::new();
            let mut app = WindowApplier::new(Cursor::new(c.base.clone()), Vec::new());
            for piece in wrapped.as_bytes().chunks(split) {
                for w in dec.feed(piece).unwrap() {
                    app.window(Some(&w)).unwrap();
                }
            }
            dec.finish().unwrap();
            app.window(None).unwrap();
            assert_eq!(app.into_output(), c.expected, "{} split {split}", c.name);
        }
    }
}

#[test]
fn applier_reports_digests_and_checks_expected_md5() {
    let c = cases()
        .into_iter()
        .find(|c| c.name == "zlib-r3.svndiff")
        .unwrap();
    let expected_md5: [u8; 16] = Md5::digest(&c.expected).into();

    let mut dec = SvndiffDecoder::new();
    let mut app =
        WindowApplier::new(Cursor::new(c.base.clone()), Vec::new()).expect_md5(expected_md5);
    let handle = app.handle();
    assert!(handle.outcome().is_none());
    dec.feed_sink(&c.svndiff, &mut app).unwrap();
    dec.finish_sink(&mut app).unwrap();
    let outcome = handle.outcome().expect("outcome");
    assert_eq!(outcome.md5, expected_md5);
    assert_eq!(outcome.bytes_written, c.expected.len() as u64);

    // Wrong checksum is rejected with Corrupt.
    let mut dec = SvndiffDecoder::new();
    let mut bad = WindowApplier::new(Cursor::new(c.base.clone()), Vec::new()).expect_md5([0; 16]);
    dec.feed_sink(&c.svndiff, &mut bad).unwrap();
    let err = dec.finish_sink(&mut bad).unwrap_err();
    assert_eq!(err.code, svn_types::ErrorCode::Corrupt);
}

#[test]
fn truncated_fixture_is_an_error_not_a_panic() {
    for c in &cases() {
        for cut in [1, 3, 5, 9, c.svndiff.len() / 2, c.svndiff.len() - 1] {
            let mut dec = SvndiffDecoder::new();
            let res = dec.feed(&c.svndiff[..cut]).and_then(|_| dec.finish());
            assert!(res.is_err(), "{} cut at {cut}", c.name);
        }
    }
}

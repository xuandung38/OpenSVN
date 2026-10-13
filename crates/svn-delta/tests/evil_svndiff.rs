//! Hostile svndiff streams from `fixtures/evil/svndiff/` (see `fixtures/evil/README.md`).
//! Each must fail with `ErrorCode::Corrupt` (decode-time or apply-time), never panic and never
//! allocate what the stream merely declares. Streams are also fed one byte at a time and through
//! the base64 wrapper used inside update reports.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Cursor;
use std::path::PathBuf;

use base64::Engine as _;
use svn_delta::{Base64SvndiffDecoder, SvndiffDecoder, WindowApplier};
use svn_types::{ErrorCode, Result, TxDeltaWindow};

fn evil_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/evil")
}

/// `(id, parser, bytes)` for every svndiff case in `cases.tsv`.
fn cases() -> Vec<(String, String, Vec<u8>)> {
    let index = std::fs::read_to_string(evil_dir().join("cases.tsv")).expect("cases.tsv");
    let out: Vec<_> = index
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| {
            let cols: Vec<&str> = l.split('\t').collect();
            matches!(cols[1], "svndiff" | "svndiff-apply").then(|| {
                assert_eq!(cols[3], "corrupt", "{}", cols[0]);
                let data = std::fs::read(evil_dir().join(cols[2])).expect(cols[2]);
                (cols[0].to_owned(), cols[1].to_owned(), data)
            })
        })
        .collect();
    assert!(out.len() >= 15, "only {} svndiff cases", out.len());
    out
}

fn decode(chunks: &mut dyn Iterator<Item = &[u8]>) -> Result<Vec<TxDeltaWindow>> {
    let mut dec = SvndiffDecoder::new();
    let mut windows = Vec::new();
    for c in chunks {
        windows.extend(dec.feed(c)?);
    }
    dec.finish()?;
    Ok(windows)
}

/// Decodes and then applies against a 16-byte base, as an update would.
fn decode_and_apply(chunks: &mut dyn Iterator<Item = &[u8]>) -> Result<Vec<u8>> {
    let windows = decode(chunks)?;
    let mut app = WindowApplier::new(Cursor::new(vec![b'x'; 16]), Vec::new());
    for w in &windows {
        app.apply_window(w)?;
    }
    app.finish()?;
    Ok(app.into_output())
}

fn expect_corrupt<T: std::fmt::Debug>(id: &str, how: &str, r: Result<T>) {
    match r {
        Ok(v) => panic!("{id} ({how}): hostile svndiff accepted: {v:?}"),
        Err(e) => assert_eq!(e.code, ErrorCode::Corrupt, "{id} ({how}): {e:?}"),
    }
}

#[test]
fn hostile_svndiff_streams_are_rejected() {
    for (id, parser, data) in cases() {
        let run: fn(&mut dyn Iterator<Item = &[u8]>) -> Result<Vec<u8>> = if parser == "svndiff" {
            |it| decode(it).map(|w| vec![0u8; w.len()])
        } else {
            decode_and_apply
        };
        expect_corrupt(&id, "whole", run(&mut std::iter::once(data.as_slice())));
        expect_corrupt(&id, "bytewise", run(&mut data.chunks(1)));
    }
}

#[test]
fn hostile_svndiff_inside_base64_txdelta_is_rejected() {
    for (id, parser, data) in cases() {
        if parser != "svndiff" {
            continue;
        }
        let text = base64::engine::general_purpose::STANDARD.encode(&data);
        let mut dec = Base64SvndiffDecoder::new();
        let r = text
            .as_bytes()
            .chunks(7)
            .try_for_each(|c| dec.feed(c).map(drop))
            .and_then(|()| dec.finish());
        expect_corrupt(&id, "base64", r);
    }
}

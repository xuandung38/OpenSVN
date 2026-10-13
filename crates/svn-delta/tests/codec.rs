//! Hand-built vectors, encoder behaviour and error paths.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Cursor;

use bytes::Bytes;
use svn_delta::{
    Base64Decoder, SvndiffDecoder, SvndiffEncoder, SvndiffVersion, WINDOW_SIZE, WindowApplier,
};
use svn_types::{ErrorCode, TxDeltaOp, TxDeltaWindow};

fn decode_all(stream: &[u8]) -> Vec<TxDeltaWindow> {
    let mut dec = SvndiffDecoder::new();
    let w = dec.feed(stream).unwrap();
    dec.finish().unwrap();
    w
}

fn apply(base: &[u8], windows: &[TxDeltaWindow]) -> Vec<u8> {
    let mut app = WindowApplier::new(Cursor::new(base.to_vec()), Vec::new());
    for w in windows {
        app.apply_window(w).unwrap();
    }
    app.finish().unwrap();
    app.into_output()
}

#[test]
fn spec_vector_v0() {
    // header; window: sview 0/0, tview 7, ins 3 bytes, new 5 bytes.
    // ops: new 5 ("hello"), target copy 2 @ 0 ("he")
    let stream = [
        b'S', b'V', b'N', 0, 0, 0, 7, 3, 5, 0x85, 0x42, 0x00, b'h', b'e', b'l', b'l', b'o',
    ];
    let windows = decode_all(&stream);
    assert_eq!(windows.len(), 1);
    assert_eq!(apply(b"", &windows), b"hellohe");
}

#[test]
fn overlapping_target_copy_repeats_pattern() {
    // "ab" then target copy of 10 bytes from offset 0 -> "ab" repeated.
    let w = TxDeltaWindow {
        sview_offset: 0,
        sview_len: 0,
        tview_len: 12,
        ops: vec![
            TxDeltaOp::NewData {
                offset: 0,
                length: 2,
            },
            TxDeltaOp::CopyFromTarget {
                offset: 0,
                length: 10,
            },
        ],
        new_data: Bytes::from_static(b"ab"),
    };
    assert_eq!(apply(b"", std::slice::from_ref(&w)), b"abababababab");
    // Same through the wire format, all versions.
    for v in [SvndiffVersion::V0, SvndiffVersion::V1, SvndiffVersion::V2] {
        let mut out = Vec::new();
        SvndiffEncoder::new(v).write_window(&w, &mut out).unwrap();
        assert_eq!(decode_all(&out), vec![w.clone()]);
    }
    // Overlap that starts mid-way: "xyz" + copy 7 from offset 1.
    let w2 = TxDeltaWindow {
        sview_offset: 0,
        sview_len: 0,
        tview_len: 10,
        ops: vec![
            TxDeltaOp::NewData {
                offset: 0,
                length: 3,
            },
            TxDeltaOp::CopyFromTarget {
                offset: 1,
                length: 7,
            },
        ],
        new_data: Bytes::from_static(b"xyz"),
    };
    assert_eq!(apply(b"", &[w2]), b"xyzyzyzyzy");
}

#[test]
fn source_view_selects_base_region() {
    let w = TxDeltaWindow {
        sview_offset: 3,
        sview_len: 4,
        tview_len: 6,
        ops: vec![
            TxDeltaOp::CopyFromSource {
                offset: 1,
                length: 3,
            },
            TxDeltaOp::NewData {
                offset: 0,
                length: 3,
            },
        ],
        new_data: Bytes::from_static(b"!!!"),
    };
    assert_eq!(apply(b"0123456789", &[w]), b"456!!!");
}

#[test]
fn view_beyond_base_is_corrupt() {
    let w = TxDeltaWindow {
        sview_offset: 8,
        sview_len: 10,
        tview_len: 1,
        ops: vec![TxDeltaOp::CopyFromSource {
            offset: 0,
            length: 1,
        }],
        new_data: Bytes::new(),
    };
    let mut app = WindowApplier::new(Cursor::new(b"0123456789".to_vec()), Vec::new());
    assert_eq!(app.apply_window(&w).unwrap_err().code, ErrorCode::Corrupt);
}

#[test]
fn op_beyond_view_is_corrupt() {
    let w = TxDeltaWindow {
        sview_offset: 0,
        sview_len: 2,
        tview_len: 3,
        ops: vec![TxDeltaOp::CopyFromSource {
            offset: 0,
            length: 3,
        }],
        new_data: Bytes::new(),
    };
    let mut app = WindowApplier::new(Cursor::new(b"0123".to_vec()), Vec::new());
    assert_eq!(app.apply_window(&w).unwrap_err().code, ErrorCode::Corrupt);
}

#[test]
fn encoder_header_and_empty_input() {
    for (v, byte) in [
        (SvndiffVersion::V0, 0),
        (SvndiffVersion::V1, 1),
        (SvndiffVersion::V2, 2),
    ] {
        let out = SvndiffEncoder::fulltext(v, b"").unwrap();
        assert_eq!(out, [b'S', b'V', b'N', byte]);
        assert!(decode_all(&out).is_empty());
    }
}

#[test]
fn fulltext_splits_into_standard_windows() {
    let data: Vec<u8> = (0..WINDOW_SIZE * 2 + 17).map(|i| (i % 251) as u8).collect();
    for v in [SvndiffVersion::V0, SvndiffVersion::V1, SvndiffVersion::V2] {
        let stream = SvndiffEncoder::fulltext(v, &data).unwrap();
        let windows = decode_all(&stream);
        assert_eq!(windows.len(), 3);
        assert!(windows.iter().all(|w| w.tview_len as usize <= WINDOW_SIZE));
        assert_eq!(apply(b"", &windows), data);
    }
}

#[test]
fn compressible_input_shrinks_and_incompressible_stays_raw() {
    let zeros = vec![0u8; 50_000];
    for v in [SvndiffVersion::V1, SvndiffVersion::V2] {
        assert!(SvndiffEncoder::fulltext(v, &zeros).unwrap().len() < 1_000);
    }
    // Pseudo-random bytes: the stored section must not grow beyond prefix overhead.
    let mut x = 0x1234_5678_u32;
    let noise: Vec<u8> = (0..4096)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 24) as u8
        })
        .collect();
    for v in [SvndiffVersion::V1, SvndiffVersion::V2] {
        let out = SvndiffEncoder::fulltext(v, &noise).unwrap();
        assert!(out.len() < noise.len() + 64);
        assert_eq!(apply(b"", &decode_all(&out)), noise);
    }
}

#[test]
fn bad_streams_are_corrupt_errors() {
    let corrupt = |bytes: &[u8]| {
        let mut dec = SvndiffDecoder::new();
        let res = dec.feed(bytes).and_then(|_| dec.finish());
        assert_eq!(res.unwrap_err().code, ErrorCode::Corrupt, "{bytes:?}");
    };
    corrupt(b"XVN\0");
    corrupt(b"SVN\x07");
    corrupt(b"SV");
    corrupt(b"SVN\0\0\0"); // window header cut
    // 5 integers present, sections missing
    corrupt(&[b'S', b'V', b'N', 0, 0, 0, 1, 1, 1]);
    // opcode 3
    corrupt(&[b'S', b'V', b'N', 0, 0, 0, 1, 1, 0, 0xc1]);
    // sizes beyond limits
    corrupt(&[b'S', b'V', b'N', 0, 0, 0xff, 0xff, 0xff, 0x7f, 0, 0, 0]);
    // v1 section declaring more bytes than it holds (zlib garbage)
    let mut garbage = vec![b'S', b'V', b'N', 1, 0, 0, 100, 3, 8, 100, 0x40, 0x00];
    garbage.extend_from_slice(&[0x64, 1, 2, 3, 4, 5, 6, 7]);
    corrupt(&garbage);
    // v2 lz4 garbage
    garbage[3] = 2;
    corrupt(&garbage);
}

#[test]
fn decoder_stays_failed_after_error() {
    let mut dec = SvndiffDecoder::new();
    assert!(dec.feed(b"nope").is_err());
    assert!(dec.feed(b"SVN\0").is_err());
    assert!(dec.finish().is_err());
}

#[test]
fn empty_stream_is_an_empty_delta() {
    let mut dec = SvndiffDecoder::new();
    assert!(dec.feed(b"").unwrap().is_empty());
    dec.finish().unwrap();
}

#[test]
fn backwards_source_view_is_rejected() {
    let mk = |off: u64| TxDeltaWindow {
        sview_offset: off,
        sview_len: 4,
        tview_len: 1,
        ops: vec![TxDeltaOp::CopyFromSource {
            offset: 0,
            length: 1,
        }],
        new_data: Bytes::new(),
    };
    let mut enc = SvndiffEncoder::new(SvndiffVersion::V0);
    let mut out = Vec::new();
    enc.write_window(&mk(10), &mut out).unwrap();
    enc.write_window(&mk(2), &mut out).unwrap();
    let mut dec = SvndiffDecoder::new();
    assert_eq!(dec.feed(&out).unwrap_err().code, ErrorCode::Corrupt);
}

#[test]
fn encoder_rejects_inconsistent_window() {
    let w = TxDeltaWindow {
        sview_offset: 0,
        sview_len: 0,
        tview_len: 5,
        ops: vec![],
        new_data: Bytes::new(),
    };
    assert!(
        SvndiffEncoder::new(SvndiffVersion::V0)
            .write_window(&w, &mut Vec::new())
            .is_err()
    );
}

#[test]
fn base64_edge_cases() {
    let mut d = Base64Decoder::new();
    assert_eq!(d.feed(b"aGVs").unwrap(), b"hel");
    assert_eq!(d.feed(b"bG8=").unwrap(), b"lo");
    d.finish().unwrap();
    assert!(d.feed(b"AAAA").is_err(), "data after padding");

    let mut d = Base64Decoder::new();
    d.feed(b"aGV").unwrap();
    assert!(d.finish().is_err(), "partial group");

    let mut d = Base64Decoder::new();
    assert!(d.feed(b"a*Vs").is_err());
}

#[test]
fn empty_delta_marker_without_windows_writes_nothing() {
    let mut app = WindowApplier::new(Cursor::new(Vec::new()), Vec::new());
    let outcome = app.finish().unwrap();
    assert_eq!(outcome.bytes_written, 0);
    // MD5 of empty input.
    assert_eq!(outcome.md5[0], 0xd4);
}

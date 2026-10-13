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
fn stream_without_header_is_rejected_but_header_alone_is_an_empty_delta() {
    let mut dec = SvndiffDecoder::new();
    assert!(dec.feed(b"").unwrap().is_empty());
    assert_eq!(dec.finish().unwrap_err().code, ErrorCode::Corrupt);

    let mut dec = SvndiffDecoder::new();
    assert!(dec.feed(b"SVN\0").unwrap().is_empty());
    dec.finish().unwrap();
}

/// Builds `SVN<version>` + one window with the given raw header integers and sections.
fn raw_window(version: u8, ints: [u64; 5], ins: &[u8], new: &[u8]) -> Vec<u8> {
    let mut out = vec![b'S', b'V', b'N', version];
    for i in ints {
        svn_delta::encode_varint(i, &mut out);
    }
    out.extend_from_slice(ins);
    out.extend_from_slice(new);
    out
}

fn assert_corrupt(stream: &[u8]) {
    let mut dec = SvndiffDecoder::new();
    let res = dec.feed(stream).and_then(|_| dec.finish());
    assert_eq!(res.unwrap_err().code, ErrorCode::Corrupt);
}

#[test]
fn window_limits_follow_subversion() {
    // 100 KiB is the limit for every view and section; one more is rejected up front.
    assert_corrupt(&raw_window(0, [0, 102_401, 1, 1, 1], &[], &[]));
    assert_corrupt(&raw_window(0, [0, 0, 102_401, 1, 1], &[], &[]));
    assert_corrupt(&raw_window(0, [0, 0, 1, 2_150_417, 0], &[], &[]));
    assert_corrupt(&raw_window(0, [0, 0, 1, 1, 102_417], &[], &[]));
}

#[test]
fn tiny_section_declaring_huge_output_is_rejected_without_allocating() {
    for version in [1u8, 2] {
        // 20 stored bytes whose length prefix claims 64 MiB.
        let mut section = Vec::new();
        svn_delta::encode_varint(64 * 1024 * 1024, &mut section);
        section.extend_from_slice(&[0x78, 0x9c, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14]);
        assert_eq!(section.len(), 20);
        let ins = [0x81u8, 0x01]; // v1/v2 prefix + 1 byte, irrelevant: new-data check fails first
        let stream = raw_window(
            version,
            [0, 0, 1, ins.len() as u64, section.len() as u64],
            &ins,
            &section,
        );
        assert_corrupt(&stream);
    }
}

#[test]
fn zlib_bomb_is_bounded_by_declared_length() {
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;
    // Compresses 16 MiB of zeros into a few dozen KiB; the section lies about its size.
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(&vec![0u8; 16 * 1024 * 1024]).unwrap();
    let bomb = enc.finish().unwrap();
    assert!(bomb.len() < 102_400, "bomb is {} bytes", bomb.len());
    for declared in [600u64, 102_400] {
        let mut section = Vec::new();
        svn_delta::encode_varint(declared, &mut section);
        section.extend_from_slice(&bomb);
        let stream = raw_window(
            1,
            [0, 0, 1, 2, section.len() as u64],
            &[0x81, 0x01],
            &section,
        );
        assert_corrupt(&stream);
    }
}

#[test]
fn compressed_section_larger_than_limit_after_decompression_is_rejected() {
    let mut section = Vec::new();
    svn_delta::encode_varint(102_401, &mut section);
    section.extend_from_slice(&[0u8; 8]);
    assert_corrupt(&raw_window(
        1,
        [0, 0, 1, 2, section.len() as u64],
        &[0x81, 0x01],
        &section,
    ));
}

#[test]
fn zero_length_ops_and_unused_new_data_are_rejected() {
    // new-data op with explicit zero length, then a real one
    assert_corrupt(&raw_window(0, [0, 0, 1, 3, 1], &[0x80, 0x00, 0x81], b"a"));
    // 2 bytes of new data supplied, only 1 used
    assert_corrupt(&raw_window(0, [0, 0, 1, 1, 2], &[0x81], b"ab"));
}

#[test]
fn applier_stays_failed_and_publishes_nothing_after_an_error() {
    let good = TxDeltaWindow::full_text(Bytes::from_static(b"abc"));
    let bad = TxDeltaWindow {
        sview_offset: 0,
        sview_len: 50,
        tview_len: 1,
        ops: vec![TxDeltaOp::CopyFromSource {
            offset: 0,
            length: 1,
        }],
        new_data: Bytes::new(),
    };
    let mut app = WindowApplier::new(Cursor::new(b"short".to_vec()), Vec::new());
    let handle = app.handle();
    app.apply_window(&good).unwrap();
    assert!(app.apply_window(&bad).is_err());
    assert!(app.apply_window(&good).is_err(), "later windows refused");
    assert!(app.finish().is_err(), "finish refused");
    assert!(handle.outcome().is_none(), "no digest after failure");

    // Checksum mismatch also leaves no outcome behind.
    let mut app = WindowApplier::new(Cursor::new(Vec::new()), Vec::new()).expect_md5([1; 16]);
    let handle = app.handle();
    app.apply_window(&good).unwrap();
    assert!(app.finish().is_err());
    assert!(handle.outcome().is_none());
    assert!(app.finish().is_err());
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

#[test]
fn large_instruction_section_up_to_the_upstream_limit_decodes() {
    // 1 byte of new data, then 102,399 one-byte target copies: 204,799 instruction bytes.
    let mut ins = vec![0x81u8];
    for _ in 0..102_399 {
        ins.extend_from_slice(&[0x41, 0x00]);
    }
    assert_eq!(ins.len(), 204_799);
    let stream = raw_window(0, [0, 0, 102_400, ins.len() as u64, 1], &ins, b"z");
    let windows = decode_all(&stream);
    assert_eq!(windows.len(), 1);
    assert_eq!(apply(b"", &windows), vec![b'z'; 102_400]);
}

#[test]
fn source_view_order_is_tracked_across_empty_views() {
    let mk = |off: u64, len: u64| TxDeltaWindow {
        sview_offset: off,
        sview_len: len,
        tview_len: 1,
        ops: vec![if len > 0 {
            TxDeltaOp::CopyFromSource {
                offset: 0,
                length: 1,
            }
        } else {
            TxDeltaOp::NewData {
                offset: 0,
                length: 1,
            }
        }],
        new_data: if len > 0 {
            Bytes::new()
        } else {
            Bytes::from_static(b"x")
        },
    };
    let encode = |views: &[(u64, u64)]| {
        let mut enc = SvndiffEncoder::new(SvndiffVersion::V0);
        let mut out = Vec::new();
        for &(o, l) in views {
            enc.write_window(&mk(o, l), &mut out).unwrap();
        }
        out
    };
    // (10,5) -> (0,0) -> (2,3): the empty view resets the baseline, as upstream does.
    assert_eq!(decode_all(&encode(&[(10, 5), (0, 0), (2, 3)])).len(), 3);
    // Without the empty view in between, going backwards is still rejected.
    let mut dec = SvndiffDecoder::new();
    assert_eq!(
        dec.feed(&encode(&[(10, 5), (2, 3)])).unwrap_err().code,
        ErrorCode::Corrupt
    );
}

//! Property tests: round trips, chunk-split independence, hostile input never panics.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Cursor;

use bytes::Bytes;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use svn_delta::{SvndiffDecoder, SvndiffEncoder, SvndiffVersion, WindowApplier};
use svn_types::{TxDeltaOp, TxDeltaWindow};

fn version() -> impl Strategy<Value = SvndiffVersion> {
    prop_oneof![
        Just(SvndiffVersion::V0),
        Just(SvndiffVersion::V1),
        Just(SvndiffVersion::V2)
    ]
}

/// Inputs mixing compressible runs and noise, so both stored and compressed sections occur.
fn payload() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        proptest::collection::vec(any::<u8>(), 0..2048),
        (proptest::collection::vec(any::<u8>(), 1..16), 1usize..400)
            .prop_map(|(unit, n)| unit.repeat(n)),
    ]
}

fn decode_in_chunks(stream: &[u8], cuts: &[usize]) -> Vec<TxDeltaWindow> {
    let mut dec = SvndiffDecoder::new();
    let mut out = Vec::new();
    let mut at = 0;
    for &c in cuts {
        let end = (at + c).min(stream.len());
        out.extend(dec.feed(&stream[at..end]).unwrap());
        at = end;
    }
    out.extend(dec.feed(&stream[at..]).unwrap());
    dec.finish().unwrap();
    out
}

fn apply(base: &[u8], windows: &[TxDeltaWindow]) -> Vec<u8> {
    let mut app = WindowApplier::new(Cursor::new(base.to_vec()), Vec::new());
    for w in windows {
        app.apply_window(w).unwrap();
    }
    app.finish().unwrap();
    app.into_output()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn fulltext_roundtrip_with_random_chunking(
        data in payload(),
        v in version(),
        cuts in proptest::collection::vec(0usize..64, 0..12),
    ) {
        let stream = SvndiffEncoder::fulltext(v, &data).unwrap();
        let windows = decode_in_chunks(&stream, &cuts);
        prop_assert_eq!(apply(b"", &windows), data);
    }
}

/// Builds a random but valid window over a base of `base_len` bytes.
fn window_strategy(base_len: usize) -> impl Strategy<Value = TxDeltaWindow> {
    let steps = proptest::collection::vec((0u8..3, 1u64..40, any::<u64>()), 1..20);
    (
        0..=base_len,
        steps,
        proptest::collection::vec(any::<u8>(), 0..400),
    )
        .prop_map(move |(sview_len, steps, pool)| {
            let mut ops = Vec::new();
            let mut tpos = 0u64;
            let mut npos = 0u64;
            let mut new_data = Vec::new();
            for (kind, len, r) in steps {
                match kind {
                    0 if sview_len > 0 => {
                        let len = len.min(sview_len as u64);
                        let offset = r % (sview_len as u64 - len + 1);
                        ops.push(TxDeltaOp::CopyFromSource {
                            offset,
                            length: len,
                        });
                        tpos += len;
                    }
                    1 if tpos > 0 => {
                        ops.push(TxDeltaOp::CopyFromTarget {
                            offset: r % tpos,
                            length: len,
                        });
                        tpos += len;
                    }
                    _ => {
                        let len = len.min(pool.len() as u64);
                        if len == 0 {
                            continue;
                        }
                        new_data.extend_from_slice(&pool[..len as usize]);
                        ops.push(TxDeltaOp::NewData {
                            offset: npos,
                            length: len,
                        });
                        npos += len;
                        tpos += len;
                    }
                }
            }
            TxDeltaWindow {
                sview_offset: 0,
                sview_len: sview_len as u64,
                tview_len: tpos,
                ops,
                new_data: Bytes::from(new_data),
            }
        })
}

/// Byte-at-a-time reference implementation of window semantics.
fn reference(base: &[u8], w: &TxDeltaWindow) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for op in &w.ops {
        match *op {
            TxDeltaOp::CopyFromSource { offset, length } => {
                for i in 0..length {
                    out.push(base[(w.sview_offset + offset + i) as usize]);
                }
            }
            TxDeltaOp::CopyFromTarget { offset, length } => {
                for i in 0..length {
                    out.push(out[(offset + i) as usize]);
                }
            }
            TxDeltaOp::NewData { offset, length } => {
                for i in 0..length {
                    out.push(w.new_data[(offset + i) as usize]);
                }
            }
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn random_windows_roundtrip_and_match_reference(
        base in proptest::collection::vec(any::<u8>(), 1..300),
        v in version(),
        seed in any::<u64>(),
        cut in 1usize..9,
    ) {
        let mut runner = proptest::test_runner::TestRunner::new_with_rng(
            ProptestConfig::default(),
            proptest::test_runner::TestRng::from_seed(
                proptest::test_runner::RngAlgorithm::ChaCha,
                &{
                    let mut s = [0u8; 32];
                    s[..8].copy_from_slice(&seed.to_le_bytes());
                    s
                },
            ),
        );
        let w = window_strategy(base.len()).new_tree(&mut runner).unwrap().current();
        let mut stream = Vec::new();
        SvndiffEncoder::new(v).write_window(&w, &mut stream).unwrap();
        let cuts = vec![cut; 6];
        let decoded = decode_in_chunks(&stream, &cuts);
        prop_assert_eq!(&decoded, &vec![w.clone()]);
        prop_assert_eq!(apply(&base, &decoded), reference(&base, &w));
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..300), v in 0u8..3) {
        let mut stream = vec![b'S', b'V', b'N', v];
        stream.extend_from_slice(&bytes);
        let mut dec = SvndiffDecoder::new();
        let mut app = WindowApplier::new(Cursor::new(vec![7u8; 64]), Vec::new());
        if let Ok(ws) = dec.feed(&stream) {
            for w in &ws {
                let _ = app.apply_window(w);
            }
        }
        let _ = dec.finish();
    }

    #[test]
    fn mutated_valid_streams_never_panic(
        data in payload(),
        v in version(),
        flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..4),
    ) {
        let mut stream = SvndiffEncoder::fulltext(v, &data).unwrap();
        for (i, x) in flips {
            let at = i % stream.len();
            stream[at] ^= x;
        }
        let mut dec = SvndiffDecoder::new();
        let _ = dec.feed(&stream);
        let _ = dec.finish();
    }
}

/// Throughput probes with small chunk sizes (the case that used to be quadratic). Run in release:
/// `cargo test -p svn-delta --release --test properties -- --ignored --nocapture`.
fn mb_per_s(bytes: usize, start: std::time::Instant) -> f64 {
    bytes as f64 / start.elapsed().as_secs_f64() / 1e6
}

fn probe_stream() -> (Vec<u8>, Vec<u8>) {
    let data: Vec<u8> = (0..32 * 1024 * 1024u32).map(|i| (i % 253) as u8).collect();
    let stream = SvndiffEncoder::fulltext(SvndiffVersion::V0, &data).unwrap();
    (data, stream)
}

#[test]
#[ignore = "benchmark"]
fn decode_throughput_by_chunk_size() {
    let (data, stream) = probe_stream();
    for chunk in [57usize, 4096, 65536] {
        let start = std::time::Instant::now();
        let mut dec = SvndiffDecoder::new();
        let mut total = 0u64;
        for piece in stream.chunks(chunk) {
            total += dec
                .feed(piece)
                .unwrap()
                .iter()
                .map(|w| w.tview_len)
                .sum::<u64>();
        }
        dec.finish().unwrap();
        let rate = mb_per_s(data.len(), start);
        println!("svndiff0 chunk {chunk:>6}: {rate:.0} MB/s");
        assert_eq!(total, data.len() as u64);
        assert!(rate >= 300.0, "chunk {chunk}: {rate:.0} MB/s < 300");
    }
}

#[test]
#[ignore = "benchmark"]
fn base64_decode_throughput_with_wrapped_lines() {
    use base64::Engine;
    let (data, stream) = probe_stream();
    let text = base64::engine::general_purpose::STANDARD.encode(&stream);
    let wrapped: Vec<u8> = text
        .as_bytes()
        .chunks(76)
        .flat_map(|l| l.iter().copied().chain(std::iter::once(b'\n')))
        .collect();
    for chunk in [77usize, 4096] {
        let start = std::time::Instant::now();
        let mut dec = svn_delta::Base64SvndiffDecoder::new();
        let mut total = 0u64;
        for piece in wrapped.chunks(chunk) {
            total += dec
                .feed(piece)
                .unwrap()
                .iter()
                .map(|w| w.tview_len)
                .sum::<u64>();
        }
        dec.finish().unwrap();
        let rate = mb_per_s(data.len(), start);
        println!("base64+svndiff0 chunk {chunk:>5}: {rate:.0} MB/s (decoded output)");
        assert_eq!(total, data.len() as u64);
        assert!(rate >= 100.0, "chunk {chunk}: {rate:.0} MB/s < 100");
    }
}

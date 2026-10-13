//! Decode and apply throughput. Run with `cargo bench -p svn-delta`.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Cursor;

use base64::Engine;
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use svn_delta::{
    Base64SvndiffDecoder, SvndiffDecoder, SvndiffEncoder, SvndiffVersion, WindowApplier,
};

const PAYLOAD: usize = 8 * 1024 * 1024;

fn payload() -> Vec<u8> {
    (0..PAYLOAD as u32).map(|i| (i % 253) as u8).collect()
}

fn bench_decode(c: &mut Criterion) {
    let data = payload();
    let mut group = c.benchmark_group("decode");
    group.throughput(Throughput::Bytes(PAYLOAD as u64));
    for version in [SvndiffVersion::V0, SvndiffVersion::V1, SvndiffVersion::V2] {
        let stream = SvndiffEncoder::fulltext(version, &data).unwrap();
        for chunk in [57usize, 4096, 65536] {
            group.bench_function(format!("{version:?}/chunk-{chunk}"), |b| {
                b.iter(|| {
                    let mut dec = SvndiffDecoder::new();
                    let mut windows = 0;
                    for piece in stream.chunks(chunk) {
                        windows += dec.feed(piece).unwrap().len();
                    }
                    dec.finish().unwrap();
                    windows
                });
            });
        }
    }
    let stream = SvndiffEncoder::fulltext(SvndiffVersion::V0, &data).unwrap();
    let text = base64::engine::general_purpose::STANDARD.encode(&stream);
    let wrapped: Vec<u8> = text
        .as_bytes()
        .chunks(76)
        .flat_map(|l| l.iter().copied().chain(std::iter::once(b'\n')))
        .collect();
    group.bench_function("V0/base64-76-char-lines", |b| {
        b.iter(|| {
            let mut dec = Base64SvndiffDecoder::new();
            let mut windows = 0;
            for line in wrapped.chunks(77) {
                windows += dec.feed(line).unwrap().len();
            }
            dec.finish().unwrap();
            windows
        });
    });
    group.finish();
}

fn bench_apply(c: &mut Criterion) {
    let data = payload();
    let stream = SvndiffEncoder::fulltext(SvndiffVersion::V0, &data).unwrap();
    let windows = SvndiffDecoder::new().feed(&stream).unwrap();
    let mut group = c.benchmark_group("apply");
    group.throughput(Throughput::Bytes(PAYLOAD as u64));
    group.bench_function("fulltext", |b| {
        b.iter_batched(
            || WindowApplier::new(Cursor::new(Vec::new()), std::io::sink()),
            |mut app| {
                for w in &windows {
                    app.apply_window(w).unwrap();
                }
                app.finish().unwrap()
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_decode, bench_apply);
criterion_main!(benches);

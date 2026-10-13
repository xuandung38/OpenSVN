//! Large transfers must not buffer bodies: peak memory stays flat for 1 GiB in both directions.
//!
//! These are slow, so they are ignored by default. Run with
//! `cargo test -p svn-ra --release --test transport_streaming -- --ignored --nocapture`.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use common::{Reply, ScriptedPrompter, TestCa, TlsServer, transport_trusting};
use reqwest::Method;
use svn_ra::transport::{HttpRequest, RequestBody};
use svn_types::CancelToken;

const GIB: u64 = 1024 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;
/// Allowed growth of resident memory over the baseline.
const BUDGET_BYTES: u64 = 50 * 1024 * 1024;

/// Resident set size of this process in bytes, via `ps` (portable across macOS and Linux).
fn rss_bytes() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    let kib: u64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("rss");
    kib * 1024
}

/// Polls RSS in the background and remembers the maximum.
fn watch_peak() -> (Arc<AtomicU64>, tokio::task::JoinHandle<()>) {
    let peak = Arc::new(AtomicU64::new(0));
    let p = Arc::clone(&peak);
    let h = tokio::spawn(async move {
        loop {
            p.fetch_max(rss_bytes(), Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    (peak, h)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "moves 2 GiB through TLS; run manually"]
async fn one_gib_download_and_upload_keep_memory_flat() {
    let ca = TestCa::new();
    let srv = TlsServer::start(
        &ca.leaf(&["localhost"], false),
        Arc::new(|seen| {
            if seen.method == Method::GET {
                Reply::zeros(GIB)
            } else {
                Reply::status(201)
            }
        }),
    )
    .await;
    let t = transport_trusting(&ca, ScriptedPrompter::with_credentials("a", "b"), |c| {
        c.read_timeout = Duration::from_secs(120);
    });

    // Warm up so TLS, pool and allocator state are part of the baseline.
    t.send(
        HttpRequest::new(Method::GET, srv.url("/warm")),
        &CancelToken::new(),
    )
    .await
    .unwrap()
    .chunk()
    .await
    .unwrap();
    let baseline = rss_bytes();
    let (peak, watcher) = watch_peak();

    // Download 1 GiB chunk by chunk.
    let started = std::time::Instant::now();
    let mut resp = t
        .send(
            HttpRequest::new(Method::GET, srv.url("/down")),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    let mut received = 0u64;
    while let Some(chunk) = resp.chunk().await.unwrap() {
        received += chunk.len() as u64;
    }
    assert_eq!(received, GIB);
    let down = started.elapsed();

    // Upload 1 GiB generated on the fly.
    let started = std::time::Instant::now();
    let stream = futures_util::stream::unfold(0u64, |sent| async move {
        if sent >= GIB {
            return None;
        }
        Some((
            Ok::<_, std::io::Error>(Bytes::from(vec![7u8; CHUNK])),
            sent + CHUNK as u64,
        ))
    });
    let mut req = HttpRequest::new(Method::PUT, srv.url("/up"));
    req.body = RequestBody::Stream {
        stream: Some(Box::pin(stream)),
        len: Some(GIB),
    };
    let resp = t.send(req, &CancelToken::new()).await.unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let up = started.elapsed();
    watcher.abort();

    let seen = srv.requests();
    assert_eq!(seen.last().unwrap().body_len, GIB);
    let growth = peak.load(Ordering::SeqCst).saturating_sub(baseline);
    println!(
        "download {down:?}, upload {up:?}, baseline {} MiB, peak growth {} MiB",
        baseline / (1024 * 1024),
        growth / (1024 * 1024)
    );
    assert!(growth < BUDGET_BYTES, "peak RSS grew by {growth} bytes");
}

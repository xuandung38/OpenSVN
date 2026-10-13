#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use futures_util::FutureExt;
use svn_types::ErrorCode;

use super::FakeEngine;
use crate::engine::OpContext;

/// The step has elapsed and the token has fired before the tick is polled again: both select
/// branches are ready at once, and the cancel must win every time (biased select).
#[tokio::test(start_paused = true)]
async fn a_cancel_that_coincides_with_the_end_of_a_step_always_wins() {
    let engine = FakeEngine::empty().with_step_delay(Duration::from_millis(10));
    for _ in 0..64 {
        let ctx = OpContext::detached();
        let tick = engine.network_tick(&ctx);
        tokio::pin!(tick);
        assert!((&mut tick).now_or_never().is_none(), "the step is running");
        tokio::time::advance(Duration::from_millis(20)).await;
        ctx.cancel.cancel();
        let err = tick.await.expect_err("cancel wins over the elapsed step");
        assert_eq!(err.code, ErrorCode::Cancelled);
    }
}

#[tokio::test(start_paused = true)]
async fn an_uncancelled_step_completes() {
    let engine = FakeEngine::empty().with_step_delay(Duration::from_millis(10));
    let ctx = OpContext::detached();
    engine.network_tick(&ctx).await.unwrap();
}

/// Every `tokio::select!` in the app states its order: an unbiased select picks a random ready
/// branch, which is how a cancelled job could still start (review N2).
#[test]
fn every_select_in_the_app_is_biased() {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let needle = ["tokio::", "select! {"].concat();
    let mut seen = 0;
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        for (at, _) in text.match_indices(&needle) {
            seen += 1;
            let after = text[at + needle.len()..].trim_start();
            assert!(
                after.starts_with("biased;"),
                "{} has an unbiased select at byte {at}",
                f.display()
            );
        }
    }
    assert!(seen >= 5, "found {seen} selects");
}

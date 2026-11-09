//! A file whose name on disk is not in NFC (macOS keeps the bytes a name was created with) is the
//! versioned file recorded in NFC: no `?` plus `!` pair, and edits to it are seen.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use common::*;
use svn_client::{StatusOptions, checkout, status};
use svn_types::{CancelToken, Depth, NodeStatus};

/// Spelling made of base letters and combining marks, as Finder or an archive tool writes it.
const NFD: &str = "Qua\u{0309}n ly\u{0301}.txt";
/// The same name in NFC, as `wc.db` keeps it.
const NFC: &str = "Qu\u{1ea3}n l\u{00fd}.txt";

async fn wc_with_nfc_file() -> (Sandbox, std::path::PathBuf) {
    let s = session();
    s.seed("a", "r1", &[put(NFC, "one\n"), put("plain.txt", "p\n")])
        .expect("seed");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    checkout(
        &ctx(),
        &s,
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    (sb, wc)
}

async fn statuses(wc: &std::path::Path) -> Vec<svn_types::WcStatus> {
    status(&ctx(), wc, &StatusOptions::default(), &CancelToken::new())
        .await
        .expect("status")
}

#[tokio::test(flavor = "multi_thread")]
async fn decomposed_name_on_disk_is_the_versioned_file() {
    let (_sb, wc) = wc_with_nfc_file().await;
    assert!(statuses(&wc).await.is_empty(), "clean after checkout");
    // The file keeps its content but takes the decomposed spelling, as one made by Finder has.
    std::fs::rename(wc.join(NFC), wc.join(NFD)).expect("rename");
    let spelled_as_given = std::fs::read_dir(&wc)
        .expect("ls")
        .flatten()
        .any(|e| e.file_name() == std::ffi::OsStr::new(NFD));
    if !spelled_as_given {
        return; // the file system folded the names, nothing to check
    }
    let got = statuses(&wc).await;
    assert!(
        got.is_empty(),
        "no unversioned or missing twin: {:?}",
        got.iter()
            .map(|s| (s.path.to_string(), s.text))
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn edit_of_a_decomposed_name_is_one_modified_item() {
    let (_sb, wc) = wc_with_nfc_file().await;
    std::fs::rename(wc.join(NFC), wc.join(NFD)).expect("rename");
    std::fs::write(wc.join(NFD), "two\n").expect("edit");
    let got = statuses(&wc).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].path.as_str(), NFC);
    assert_eq!(got[0].text, NodeStatus::Modified);
}

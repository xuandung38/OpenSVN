//! A property conflict on a file whose text the update leaves alone (found by
//! `update_local_edits_prop`).
//!
//! Only the `.prej` marker is written; the working file is not touched, so svn 1.15 keeps the
//! BASE row's recorded size and timestamp (`translated_size`, `last_mod_time`). We dropped them
//! because the marker's work item counted as work on the file: wc.db differed from svn and the
//! next status had to read the file again.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::Path;

use common::oracle::*;
use common::*;
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth};

/// Local and incoming changes of property `p` of `f` that conflict: `(name, local, server)`,
/// where `None` deletes the property.
const CASES: [(&str, Option<&str>, Option<&str>); 2] = [
    ("local set, incoming set", Some("mine"), Some("theirs")),
    ("local delete, incoming set", None, Some("theirs")),
];

#[tokio::test(flavor = "multi_thread")]
async fn prop_conflict_keeps_the_fileinfo_like_svn() {
    for (name, local, server) in CASES {
        let Some(oracle) = oracle() else { return };
        let repo = Repo::new(&oracle);
        let seed = repo.checkout("seed");
        write(&seed.join("f"), "one\ntwo\n");
        repo.svn(&seed, &["add", "-q", "f"]);
        repo.svn(&seed, &["propset", "-q", "p", "old", "f"]);
        repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
        let theirs = repo.checkout("svnwc");
        let sb = Sandbox::new();
        let ours = sb.path("wc");
        checkout(
            &ctx(),
            &repo.fake(Some(1)),
            &ours,
            None,
            Depth::Infinity,
            false,
            &CancelToken::new(),
        )
        .await
        .expect("checkout r1");
        for wc in [&theirs, &ours] {
            match local {
                Some(v) => repo.svn(wc, &["propset", "-q", "p", v, "f"]),
                None => repo.svn(wc, &["propdel", "-q", "p", "f"]),
            };
        }
        match server {
            Some(v) => repo.svn(&seed, &["propset", "-q", "p", v, "f"]),
            None => repo.svn(&seed, &["propdel", "-q", "p", "f"]),
        };
        repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
        repo.svn(&theirs, &["update", "-q", "--accept", "postpone"]);
        update(
            &ctx(),
            &repo.fake(None),
            &ours,
            None,
            Depth::Unknown,
            &CancelToken::new(),
        )
        .await
        .expect("update");
        assert!(ours.join("f.prej").exists(), "{name}: no property conflict");
        assert_same(&oracle, &theirs, &ours, name);
    }
}

/// The same with the fake session alone, so it runs without svn too.
#[tokio::test(flavor = "multi_thread")]
async fn prop_conflict_keeps_the_fileinfo() {
    let s = session();
    s.seed(
        "a",
        "r1",
        &[put("f", "one\ntwo\n"), prop("f", "p", Some("old"))],
    )
    .expect("seed");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let cancel = CancelToken::new();
    checkout(&ctx(), &s, &wc, None, Depth::Infinity, false, &cancel)
        .await
        .expect("checkout");
    let size_of = |wc: &Path| {
        open_db(wc)
            .read_base(&rp("f"))
            .expect("read")
            .expect("node")
            .translated_size
    };
    assert_eq!(size_of(&wc), Some(8));
    open_db(&wc)
        .set_props(&rp("f"), &vec![(b"p".to_vec(), b"mine".to_vec())], &[])
        .expect("propset");
    s.seed("a", "r2", &[prop("f", "p", Some("theirs"))])
        .expect("seed");
    update(&ctx(), &s, &wc, None, Depth::Unknown, &cancel)
        .await
        .expect("update");
    assert!(wc.join("f.prej").exists(), "no property conflict");
    assert_eq!(size_of(&wc), Some(8), "the fileinfo was dropped");
}

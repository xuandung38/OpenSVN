//! Keyword expansion after a commit must match the expansion an update produces for the same
//! BASE node. svn expands `$Date$` in local time on both paths, so the post-commit `file-commit`
//! work item and the update's `file-install` must agree byte for byte.
//!
//! The test re-runs itself in a child process with `TZ` set to a fixed non-UTC zone (the
//! workspace forbids the `unsafe` needed to change the environment in-process), so a path that
//! formats in UTC cannot pass by accident on a UTC machine.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use common::*;
use svn_client::commit::{CommitContext, CommitOptions, commit};
use svn_client::local::{self, AddConfig, AddOptions, PropSetOptions};
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth, MemoryLockOwnership};
use svn_wc::db::LockOwner;

const CHILD_ENV: &str = "OPENSVN_KEYWORD_TZ_CHILD";
const TEST_NAME: &str = "committed_and_updated_files_expand_keywords_identically";

#[test]
fn committed_and_updated_files_expand_keywords_identically() {
    if std::env::var_os(CHILD_ENV).is_none() {
        let exe = std::env::current_exe().expect("current exe");
        let out = Command::new(exe)
            .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            // POSIX TZ string: seven hours east of UTC, no DST, no tzdata needed.
            .env("TZ", "XST-7")
            .output()
            .expect("spawn child test");
        assert!(
            out.status.success(),
            "child failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("1 passed"),
            "child ran no test: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        return;
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(body());
}

async fn body() {
    let s = session();
    s.seed("alice", "r1", &[put("other.txt", "o\n")])
        .expect("seed");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let cancel = CancelToken::new();
    checkout(&ctx(), &s, &wc, None, Depth::Infinity, false, &cancel)
        .await
        .expect("checkout");

    let db = open_db(&wc);
    db.set_lock_owner(LockOwner {
        store: Arc::new(MemoryLockOwnership::for_current_process("test-boot")),
        pid: std::process::id(),
        boot_id: "test-boot".to_owned(),
    });
    let kw = wc.join("kw.txt");
    write(&kw, "$Date$\n$Rev$\n$Author$\n$Id$\n");
    local::add(
        &db,
        &cancel,
        std::slice::from_ref(&kw),
        &AddOptions::default(),
        &AddConfig::default(),
    )
    .expect("add");
    local::propset(
        &db,
        &cancel,
        &rp("kw.txt"),
        "svn:keywords",
        Some(b"Date Rev Author Id"),
        PropSetOptions::default(),
    )
    .expect("propset");
    let state: PathBuf = sb.path("state");
    let res = commit(
        &db,
        &CommitContext {
            ra: &s,
            state_dir: &state,
            cancel: &cancel,
        },
        std::slice::from_ref(&wc),
        &CommitOptions::new("add kw"),
    )
    .await
    .expect("commit")
    .expect("something to commit");
    assert_eq!(res.revision.0, 2);
    assert!(db.work_items().expect("wq").is_empty());

    let committed = read(&kw);
    assert!(committed.contains("$Rev: 2 $"), "{committed}");
    assert!(
        committed.contains(" +0700 ("),
        "$Date$ is expanded in local time: {committed}"
    );

    // Another revision on the server, then an update that only bumps kw.txt: unchanged.
    s.seed("bob", "r3", &[put("other.txt", "o2\n")])
        .expect("seed r3");
    update(&ctx(), &s, &wc, None, Depth::Unknown, &cancel)
        .await
        .expect("update");
    assert_eq!(read(&wc.join("other.txt")), "o2\n");
    assert_eq!(
        read(&kw),
        committed,
        "update leaves the committed text alone"
    );

    // The update restores a removed file through the work queue's file-install, which expands
    // keywords from the same BASE row: the text must be identical to the committed one.
    std::fs::remove_file(&kw).expect("rm kw");
    update(&ctx(), &s, &wc, None, Depth::Unknown, &cancel)
        .await
        .expect("update restores");
    assert_eq!(
        read(&kw),
        committed,
        "restored file expands keywords identically"
    );
}

fn write(path: &std::path::Path, text: &str) {
    std::fs::write(path, text).expect("write");
}

//! Update below a versioned directory replaced by a symlink to a directory outside the working
//! copy (#150), over fake repositories: the cases the svn oracle cannot share (svn follows the
//! link), checked for what matters: the update completes, no directory stays `incomplete`, and
//! the directory behind the link stays byte-identical, externals defined below it included.
#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::Path;

use common::*;
use svn_client::{ExternalAction, UpdateSummary, checkout, update};
use svn_ra_api::fake::FakeRaSession;
use svn_types::{CancelToken, Depth, Revnum};

/// Every entry below `dir` (symlinks not followed): kind, content, mode and mtime.
fn snapshot(dir: &Path) -> BTreeMap<String, (String, Vec<u8>, u32, i64, i64)> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, (String, Vec<u8>, u32, i64, i64)>) {
        for e in fs::read_dir(dir).expect("read_dir") {
            let path = e.expect("entry").path();
            let rel = path
                .strip_prefix(root)
                .expect("strip")
                .to_string_lossy()
                .into_owned();
            let md = fs::symlink_metadata(&path).expect("md");
            let (kind, content) = if md.file_type().is_symlink() {
                (
                    "link",
                    fs::read_link(&path)
                        .expect("link")
                        .into_os_string()
                        .into_encoded_bytes(),
                )
            } else if md.is_dir() {
                ("dir", Vec::new())
            } else {
                ("file", fs::read(&path).expect("read"))
            };
            out.insert(
                rel,
                (
                    kind.to_owned(),
                    content,
                    md.mode(),
                    md.mtime(),
                    md.mtime_nsec(),
                ),
            );
            if md.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// BASE rows as `relpath presence revision`, and whether any working row exists.
fn rows(wc: &Path) -> Vec<String> {
    base_rows(&open_db(wc))
        .into_iter()
        .map(|(p, presence, _, rev, _)| format!("{p} {presence} {rev}"))
        .collect()
}

fn assert_no_incomplete(wc: &Path, what: &str) {
    let r = rows(wc);
    assert!(
        !r.iter().any(|l| l.contains(" incomplete ")),
        "{what}: incomplete rows: {r:?}"
    );
}

/// r1: `d` with a file, a subdirectory and an external `d/ext` (`^/lib`), next to `keep`;
/// r2: an add, an edit and a delete below `d`, a new definition on `d`, a change in `lib`, an
/// add in `keep`; r3: `d` deleted.
fn repo() -> FakeRaSession {
    let s = session();
    s.seed(
        "alice",
        "r1",
        &[
            put("lib/a.txt", "alpha\n"),
            put("d/in.txt", "in\n"),
            put("d/gone.txt", "gone\n"),
            put("d/sub/s.txt", "s\n"),
            prop("d", "svn:externals", Some("^/lib ext\n")),
            put("keep/k.txt", "k\n"),
        ],
    )
    .unwrap();
    s.seed(
        "alice",
        "r2",
        &[
            put("d/new.txt", "new\n"),
            put("d/sub/s.txt", "s\ns2\n"),
            del("d/gone.txt"),
            prop("d", "svn:externals", Some("^/lib ext\n^/lib ext2\n")),
            put("lib/a.txt", "alpha 2\n"),
            put("keep/k2.txt", "k2\n"),
        ],
    )
    .unwrap();
    s.seed("alice", "r3", &[del("d")]).unwrap();
    s
}

async fn up(s: &FakeRaSession, wc: &Path, rev: u64) -> UpdateSummary {
    update(
        &ext_ctx(FakeOpener::new(vec![s.clone()])),
        s,
        wc,
        Some(Revnum(rev)),
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .unwrap_or_else(|e| panic!("update to r{rev}: {e}"))
}

/// Checks out r1 and replaces `d` by a symlink to its own content moved next to the working
/// copy, with a local edit there. Returns `(wc, outside)`.
async fn linked(s: &FakeRaSession, sb: &Sandbox) -> (std::path::PathBuf, std::path::PathBuf) {
    let wc = sb.path("wc");
    let outside = sb.path("outside");
    checkout(
        &ext_ctx(FakeOpener::new(vec![s.clone()])),
        s,
        &wc,
        Some(Revnum(1)),
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout r1");
    assert!(
        wc.join("d/ext/a.txt").is_file(),
        "the external is checked out"
    );
    fs::rename(wc.join("d"), &outside).expect("move d out");
    fs::write(outside.join("in.txt"), "edited behind the link\n").expect("edit");
    symlink(&outside, wc.join("d")).expect("link");
    (wc, outside)
}

#[tokio::test(flavor = "multi_thread")]
async fn update_never_writes_behind_a_symlinked_directory() {
    let s = repo();
    let sb = Sandbox::new();
    let (wc, outside) = linked(&s, &sb).await;
    let before = snapshot(&outside);

    let sum = up(&s, &wc, 2).await;
    assert_no_incomplete(&wc, "r2");
    // The rest of the tree is updated.
    assert_eq!(read(&wc.join("keep/k2.txt")), "k2\n");
    // BASE below `d` moved on; the disk there was left alone.
    let r = rows(&wc);
    for l in ["d normal 2", "d/new.txt normal 2", "d/sub/s.txt normal 2"] {
        assert!(r.iter().any(|x| x == l), "missing '{l}': {r:?}");
    }
    assert!(!r.iter().any(|x| x.starts_with("d/gone.txt ")), "{r:?}");
    let conflict = open_db(&wc).conflict(&rp("d")).unwrap();
    assert!(conflict.is_some(), "no tree conflict on d");
    // The externals defined on `d` would be written through the link: they fail, the others
    // are processed.
    let below: Vec<_> = sum
        .externals
        .iter()
        .filter(|e| e.path.as_str().starts_with("d/"))
        .map(|e| (e.path.as_str(), e.action))
        .collect();
    assert_eq!(
        below,
        [
            ("d/ext", ExternalAction::Failed),
            ("d/ext2", ExternalAction::Failed)
        ],
        "{:?}",
        sum.externals
    );
    assert_eq!(snapshot(&outside), before, "r2 wrote behind the link");

    // r3 deletes `d`: it is conflicted, so it is skipped, and nothing is written either.
    up(&s, &wc, 3).await;
    assert_no_incomplete(&wc, "r3");
    assert!(
        fs::symlink_metadata(wc.join("d"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(snapshot(&outside), before, "r3 wrote behind the link");
}

/// The state #150 left behind before the fix (the root and `d` `incomplete` at r2, `d/in.txt`
/// still at r1), resumed by the fixed engine over the corrected fake RA (#151): to r2 it
/// completes with the tree conflict, to r3 the deleted `d` stays as a conflicted local copy.
#[tokio::test(flavor = "multi_thread")]
async fn update_resumes_what_the_failed_update_left() {
    for to in [2, 3] {
        let s = repo();
        let sb = Sandbox::new();
        let (wc, outside) = linked(&s, &sb).await;
        let before = snapshot(&outside);
        open_db(&wc)
            .with_write(|tx| {
                tx.execute(
                    "UPDATE nodes SET presence = 'incomplete', revision = 2 \
                     WHERE local_relpath IN ('', 'd') AND op_depth = 0",
                    [],
                )
                .unwrap();
                Ok(())
            })
            .unwrap();

        up(&s, &wc, to).await;
        assert_no_incomplete(&wc, &format!("to r{to}"));
        assert!(
            open_db(&wc).conflict(&rp("d")).unwrap().is_some(),
            "to r{to}: no tree conflict on d"
        );
        if to == 3 {
            let r = rows(&wc);
            assert!(!r.iter().any(|x| x.starts_with("d")), "to r3: {r:?}");
            assert!(open_db(&wc).read_node(&rp("d")).unwrap().is_some());
        }
        assert_eq!(
            snapshot(&outside),
            before,
            "to r{to}: wrote behind the link"
        );
    }
}

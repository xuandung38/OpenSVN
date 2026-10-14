//! Report of a working copy left by an interrupted update (#119): an `incomplete` directory is
//! reported `start_empty`, so every one of its children must be reported, including children at
//! the directory's own revision. Otherwise mod_dav_svn sees `bin/blob1.bin` below a `bin` it
//! does not know and rejects the report, and the update can never be resumed.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;

use common::*;
use svn_client::checkout;
use svn_client::update::build_report;
use svn_types::{CancelToken, Depth, RelPath, RepoUrl, Reporter, Result, Revnum};

/// `(path, rev, start_empty)` of every `set_path`, plus deleted and linked paths.
#[derive(Default)]
struct Recorder {
    set: Vec<(String, u64, bool)>,
    other: Vec<String>,
}

impl Reporter for Recorder {
    fn set_path(
        &mut self,
        path: &RelPath,
        rev: Revnum,
        _depth: Depth,
        start_empty: bool,
        _lock_token: Option<&str>,
    ) -> Result<()> {
        self.set
            .push((path.as_str().to_owned(), rev.0, start_empty));
        Ok(())
    }
    fn delete_path(&mut self, path: &RelPath) -> Result<()> {
        self.other.push(format!("delete {path}"));
        Ok(())
    }
    fn link_path(
        &mut self,
        path: &RelPath,
        _url: &RepoUrl,
        _rev: Revnum,
        _depth: Depth,
        _start_empty: bool,
        _lock_token: Option<&str>,
    ) -> Result<()> {
        self.other.push(format!("link {path}"));
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
    fn abort(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Sets presence and revision of BASE rows, the way a killed update leaves them.
fn set_rows(db: &svn_wc::db::WcDb, rows: &[(&str, &str, i64)]) {
    db.with_write(|tx| {
        for (path, presence, rev) in rows {
            let n = tx
                .execute(
                    "UPDATE nodes SET presence = ?1, revision = ?2 \
                     WHERE local_relpath = ?3 AND op_depth = 0",
                    (presence, rev, path),
                )
                .unwrap();
            assert_eq!(n, 1, "{path}");
        }
        Ok(())
    })
    .expect("rewrite rows");
}

/// Every reported path must hang below a parent the server knows about: a reported ancestor
/// that is not `start_empty`, or the reported parent itself (what mod_dav_svn checks).
fn assert_server_can_place(report: &[(String, u64, bool)]) {
    for (path, _, _) in report {
        if path.is_empty() {
            continue;
        }
        let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
        let parent_reported = report.iter().any(|(p, _, _)| p == parent);
        let mut covered = parent_reported;
        let mut anc = parent;
        while !covered && !anc.is_empty() {
            anc = anc.rsplit_once('/').map_or("", |(p, _)| p);
            covered = report.iter().any(|(p, _, empty)| p == anc && !*empty);
        }
        assert!(
            covered,
            "'{path}' reported below an unknown parent: {report:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn incomplete_directory_reports_every_child() {
    let s = session();
    s.seed(
        "alice",
        "r1",
        &[
            put("bin/blob0.bin", "zero\n"),
            put("bin/blob1.bin", "one\n"),
            put("d0/a.txt", "a\n"),
            put("d1/b.txt", "b\n"),
            put("top.txt", "top\n"),
        ],
    )
    .expect("r1");
    s.seed(
        "bob",
        "r2",
        &[
            put("bin/blob1.bin", "one 2\n"),
            put("d0/a.txt", "a 2\n"),
            put("d1/b.txt", "b 2\n"),
        ],
    )
    .expect("r2");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let cancel = CancelToken::new();
    checkout(
        &ctx(),
        &s,
        &wc,
        Some(Revnum(1)),
        Depth::Infinity,
        false,
        &cancel,
    )
    .await
    .expect("checkout r1");
    let db = open_db(&wc);
    // A kill mid-update to r2: the root and d0 are incomplete at r2, bin was opened at r2 and
    // blob0 (unchanged in r2) bumped, blob1 not yet; d1 and top.txt were not reached.
    set_rows(
        &db,
        &[
            ("", "incomplete", 2),
            ("bin", "normal", 2),
            ("bin/blob0.bin", "normal", 2),
            ("bin/blob1.bin", "normal", 1),
            ("d0", "incomplete", 2),
            ("d0/a.txt", "normal", 1),
            ("d1", "normal", 1),
            ("d1/b.txt", "normal", 1),
            ("top.txt", "normal", 1),
        ],
    );
    let db = Arc::new(db);
    let report = build_report(
        Arc::clone(&db),
        RelPath::root(),
        RelPath::root(),
        Depth::Unknown,
        false,
        ROOT_URL.to_owned(),
    );
    let mut rec = Recorder::default();
    report(&mut rec).expect("report");
    assert!(rec.other.is_empty(), "{:?}", rec.other);
    let set = &rec.set;
    let find = |p: &str| set.iter().find(|(q, _, _)| q == p).cloned();
    assert_eq!(find(""), Some((String::new(), 2, true)));
    // Children of the incomplete root, whatever their revision.
    assert_eq!(find("bin"), Some(("bin".into(), 2, false)));
    assert_eq!(find("d0"), Some(("d0".into(), 2, true)));
    assert_eq!(find("d1"), Some(("d1".into(), 1, false)));
    assert_eq!(find("top.txt"), Some(("top.txt".into(), 1, false)));
    // Below the complete `bin`, only the child that differs.
    assert_eq!(
        find("bin/blob1.bin"),
        Some(("bin/blob1.bin".into(), 1, false))
    );
    assert_eq!(find("bin/blob0.bin"), None);
    // Below the incomplete `d0`, every child.
    assert_eq!(find("d0/a.txt"), Some(("d0/a.txt".into(), 1, false)));
    // Below the complete `d1` at r1, nothing more.
    assert_eq!(find("d1/b.txt"), None);
    assert_server_can_place(set);

    // And the interrupted update resumes.
    let db = Arc::try_unwrap(db).ok().expect("sole owner");
    drop(db);
    svn_client::update(&ctx(), &s, &wc, None, Depth::Unknown, &cancel)
        .await
        .expect("resume update");
    for (f, text) in [
        ("bin/blob0.bin", "zero\n"),
        ("bin/blob1.bin", "one 2\n"),
        ("d0/a.txt", "a 2\n"),
        ("d1/b.txt", "b 2\n"),
        ("top.txt", "top\n"),
    ] {
        assert_eq!(read(&wc.join(f)), text, "{f}");
    }
    let rows = base_rows(&open_db(&wc));
    assert!(rows.iter().all(|r| r.1 == "normal" && r.3 == 2), "{rows:?}");
}

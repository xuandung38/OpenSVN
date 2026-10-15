//! Local operations on a directory whose child was already moved away, and on the delete layer
//! of such a child (found by `update_local_edits_prop`).
//!
//! svn keeps the child moved away: after `svn mv d/sub s1; svn mv d d2` the delete layer of
//! `d2/sub` is the source of the move to `s1`; after `svn mv d/sub s1; svn rm d` the new delete
//! layer of `d/sub` is; and a node added or moved onto a moved-away child's delete layer (a
//! replacement) keeps that layer's `moved_to`. We used to drop the `moved_to` in all three
//! cases, and removing the old layer then detached the destination (no longer "moved here"):
//! the move became a plain delete and a plain copy. A move whose destination lies inside the
//! deleted directory is broken, as in svn.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
mod support;

use std::path::Path;

use support::*;
use svn_client::local::{self, DeleteOptions};
use svn_types::CancelToken;

/// `(name, svn commands, the same through svn_client::local)`.
type Case = (&'static str, &'static [&'static [&'static str]], fn(&Path));

const CASES: [Case; 5] = [
    (
        "mv parent",
        &[&["mv", "-q", "d/sub", "s1"], &["mv", "-q", "d", "d2"]],
        |wc| {
            mv(wc, "d/sub", "s1");
            mv(wc, "d", "d2");
        },
    ),
    (
        "rm parent",
        &[&["mv", "-q", "d/sub", "s1"], &["rm", "-q", "d"]],
        |wc| {
            mv(wc, "d/sub", "s1");
            rm(wc, "d");
        },
    ),
    (
        "rm parent of a move inside it",
        &[
            &["mv", "-q", "d/sub", "d/s1"],
            &["rm", "-q", "--force", "d"],
        ],
        |wc| {
            mv(wc, "d/sub", "d/s1");
            rm(wc, "d");
        },
    ),
    (
        "rm parent of a replaced moved-away child",
        &[
            &["mv", "-q", "d/f1", "s1"],
            &["add", "-q", "d/f1"],
            &["rm", "-q", "--force", "d"],
        ],
        |wc| {
            mv(wc, "d/f1", "s1");
            add(wc, "d/f1");
            rm(wc, "d");
        },
    ),
    (
        "mv onto the delete layer of a moved-away child",
        &[
            &["mv", "-q", "d/sub/g", "s1"],
            &["mv", "-q", "d/sub", "s2"],
            &["mv", "-q", "top.txt", "s2/g"],
        ],
        |wc| {
            mv(wc, "d/sub/g", "s1");
            mv(wc, "d/sub", "s2");
            mv(wc, "top.txt", "s2/g");
        },
    ),
];

/// Writes a new file at `path` and adds it.
fn add(wc: &Path, path: &str) {
    write(wc, path, "new\n");
    let db = open(wc);
    local::add(
        &db,
        &CancelToken::new(),
        &[wc.join(path)],
        &local::AddOptions::default(),
        &local::AddConfig::default(),
    )
    .expect("add");
}

fn mv(wc: &Path, from: &str, to: &str) {
    let db = open(wc);
    local::move_path(&db, &CancelToken::new(), &wc.join(from), &wc.join(to)).expect("mv");
    assert!(db.work_items().expect("queue").is_empty());
}

fn rm(wc: &Path, path: &str) {
    let db = open(wc);
    let opts = DeleteOptions {
        force: true,
        ..DeleteOptions::default()
    };
    local::delete(&db, &CancelToken::new(), &[wc.join(path)], &opts).expect("rm");
    assert!(db.work_items().expect("queue").is_empty());
}

/// Every row that takes part in a move.
fn moves(wc: &Path) -> Vec<String> {
    let c = rusqlite::Connection::open(wc.join(".svn/wc.db")).expect("open");
    let mut stmt = c
        .prepare(
            "SELECT local_relpath, op_depth, moved_here, moved_to FROM nodes \
             WHERE moved_here IS NOT NULL OR moved_to IS NOT NULL \
             ORDER BY local_relpath, op_depth",
        )
        .expect("prepare");
    stmt.query_map([], |r| {
        Ok(format!(
            "{} {} {:?} {:?}",
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, Option<String>>(3)?
        ))
    })
    .expect("query")
    .map(|r| r.expect("row"))
    .collect()
}

#[test]
fn a_moved_away_child_stays_moved_away_like_svn() {
    for (name, svn_cmds, ours_ops) in CASES {
        let Some(fx) = fixture() else { return };
        let theirs = checkout(&fx.oracle, &fx.base, &fx.url, "svn-side");
        let ours = checkout(&fx.oracle, &fx.base, &fx.url, "our-side");
        for cmd in svn_cmds {
            if cmd[0] == "add" {
                write(&theirs, cmd[2], "new\n");
            }
            svn(&fx.oracle, &theirs, cmd);
        }
        ours_ops(&ours);
        assert_rows_equal(name, &dump_rows(&theirs), &dump_rows(&ours));
        assert_eq!(moves(&ours), moves(&theirs), "{name}: move columns differ");
        assert_eq!(
            status_xml(&fx.oracle, &ours).replace("our-side", "X"),
            status_xml(&fx.oracle, &theirs).replace("svn-side", "X"),
            "{name}: svn status differs"
        );
    }
}

/// The same on checkouts from the fake session, so it runs without svn too.
#[tokio::test(flavor = "multi_thread")]
async fn a_moved_away_child_stays_moved_away() {
    let expected: [&[(&str, Option<&str>)]; 5] = [
        &[("d2/sub", Some("s1")), ("d", Some("d2"))],
        &[("d/sub", Some("s1"))],
        &[("d/sub", None)],
        &[("d/f1", Some("s1"))],
        &[("s2/g", Some("s1")), ("top.txt", Some("s2/g"))],
    ];
    for ((name, _, ours_ops), want) in CASES.into_iter().zip(expected) {
        let s = common::session();
        s.seed(
            "a",
            "r1",
            &[
                common::put("d/f1", "1\n"),
                common::put("d/sub/g", "3\n"),
                common::put("top.txt", "t\n"),
            ],
        )
        .expect("seed");
        let sb = common::Sandbox::new();
        let wc = sb.path("wc");
        svn_client::checkout(
            &common::ctx(),
            &s,
            &wc,
            None,
            svn_types::Depth::Infinity,
            false,
            &CancelToken::new(),
        )
        .await
        .expect("checkout");
        ours_ops(&wc);
        let db = open(&wc);
        for (src, dst) in want {
            assert_eq!(
                db.moved_to(&rp(src)).expect("read"),
                dst.map(rp),
                "{name}: {src}"
            );
            if let Some(dst) = dst {
                assert_eq!(
                    db.moved_from(&rp(dst)).expect("read"),
                    Some(rp(src)),
                    "{name}: {dst}"
                );
            }
        }
    }
}

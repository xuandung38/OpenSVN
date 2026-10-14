//! `mark_move` against svn 1.15: TortoiseSVN's "Repair move" (revert what svn recorded for the
//! two paths, rename the destination back, `svn move`) on one checkout, `svn_client::local::
//! mark_move` without touching the disk on another. The wc.db rows, `svn status` and the files
//! on disk must be identical. Skipped when no svn 1.15 is installed (fails instead when
//! `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod support;

use std::fs;
use std::path::Path;

use opensvn_svn_oracle::Oracle;
use support::*;
use svn_client::local;
use svn_types::{CancelToken, ErrorCode};
use svn_wc::db::MarkMove;

fn tree(wc: &Path) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = fs::read_dir(dir).expect("rd").flatten().collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for e in entries {
            if e.file_name() == ".svn" {
                continue;
            }
            let rel = e
                .path()
                .strip_prefix(base)
                .expect("prefix")
                .display()
                .to_string();
            if e.path().is_dir() {
                out.push(format!("D {rel}"));
                walk(&e.path(), base, out);
            } else {
                out.push(format!(
                    "F {rel} {:?}",
                    fs::read(e.path()).unwrap_or_default()
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(wc, wc, &mut out);
    out
}

/// First column of `svn status` for `rel` (`' '` when unmodified).
fn status_char(o: &Oracle, wc: &Path, rel: &str) -> char {
    let out = o
        .svn()
        .args(["status", "--depth", "empty", rel])
        .current_dir(wc)
        .output()
        .expect("svn status");
    String::from_utf8_lossy(&out.stdout)
        .chars()
        .next()
        .unwrap_or(' ')
}

/// The repair TortoiseSVN does, run with the real svn: revert what svn recorded for the two
/// paths (a scheduled delete, a plain addition), put the destination back at the source and
/// `svn move` it.
fn repair_move(o: &Oracle, wc: &Path, from: &str, to: &str, extra: &[&str]) {
    if status_char(o, wc, from) == 'D' {
        svn(o, wc, &["revert", "-q", "-R", from]);
    }
    if status_char(o, wc, to) == 'A' {
        svn(o, wc, &["revert", "-q", "-R", to]);
    }
    let (f, t) = (wc.join(from), wc.join(to));
    if f.is_dir() {
        fs::remove_dir_all(&f).expect("rm restored source");
    } else if f.exists() {
        fs::remove_file(&f).expect("rm restored source");
    }
    fs::rename(&t, &f).expect("rename back");
    let mut args = vec!["mv", "-q"];
    args.extend_from_slice(extra);
    args.extend([from, to]);
    svn(o, wc, &args);
}

struct Case<'a> {
    name: &'a str,
    /// Same local changes in both working copies (the move "outside version control").
    prep: &'a dyn Fn(&Oracle, &Path),
    from: &'a str,
    to: &'a str,
    /// Extra `svn move` options on the svn side.
    svn_extra: &'a [&'a str],
    /// What the svn side runs instead of the repair (after `prep`).
    theirs: Option<&'a dyn Fn(&Oracle, &Path)>,
    expect: MarkMove,
}

fn check(c: &Case<'_>) {
    let Some(fx) = fixture() else { return };
    let a = checkout(&fx.oracle, &fx.base, &fx.url, "svn-side");
    let b = checkout(&fx.oracle, &fx.base, &fx.url, "our-side");
    (c.prep)(&fx.oracle, &a);
    (c.prep)(&fx.oracle, &b);
    match c.theirs {
        Some(f) => f(&fx.oracle, &a),
        None => repair_move(&fx.oracle, &a, c.from, c.to, c.svn_extra),
    }
    let db = open(&b);
    let cancel = CancelToken::new();
    let before = tree(&b);
    let got = local::mark_move(&db, &cancel, &b.join(c.from), &b.join(c.to))
        .unwrap_or_else(|e| panic!("{}: mark_move failed: {e}", c.name));
    assert_eq!(got, c.expect, "{}", c.name);
    assert_eq!(tree(&b), before, "{}: the disk must not change", c.name);
    assert!(db.work_items().expect("wq").is_empty(), "{}: queue", c.name);
    // Recording the same move again changes nothing.
    if got == MarkMove::Moved {
        let again = local::mark_move(&db, &cancel, &b.join(c.from), &b.join(c.to))
            .expect("mark_move again");
        assert_eq!(again, MarkMove::AlreadyMoved, "{}", c.name);
    }
    drop(db);
    assert_rows_equal(c.name, &dump_rows(&a), &dump_rows(&b));
    let (sa, sb) = (
        status_xml(&fx.oracle, &a).replace("svn-side", "X"),
        status_xml(&fx.oracle, &b).replace("our-side", "X"),
    );
    assert_eq!(sa, sb, "{}: svn status differs", c.name);
    assert_eq!(tree(&a), tree(&b), "{}: files on disk differ", c.name);
    // svn accepts the result and commits it as a move (a mixed-revision delete is out of
    // date for svn as well).
    if c.expect == MarkMove::Moved {
        svn(&fx.oracle, &b, &["commit", "-q", "-m", "repaired move"]);
    }
}

fn rename(wc: &Path, from: &str, to: &str) {
    fs::rename(wc.join(from), wc.join(to)).expect("rename");
}

#[test]
fn renamed_file() {
    check(&Case {
        name: "file",
        prep: &|_, wc| rename(wc, "top.txt", "moved.txt"),
        from: "top.txt",
        to: "moved.txt",
        svn_extra: &[],
        theirs: None,
        expect: MarkMove::Moved,
    });
}

#[test]
fn renamed_and_edited_file_with_local_props() {
    check(&Case {
        name: "file-edited",
        prep: &|o, wc| {
            svn(o, wc, &["propset", "-q", "k", "local", "d/f1"]);
            rename(wc, "d/f1", "d/sub/f1-moved");
            write(wc, "d/sub/f1-moved", "1\nedited\n");
        },
        from: "d/f1",
        to: "d/sub/f1-moved",
        svn_extra: &[],
        theirs: None,
        expect: MarkMove::Moved,
    });
}

#[test]
fn renamed_directory_with_edits_and_new_files() {
    check(&Case {
        name: "dir",
        prep: &|_, wc| {
            rename(wc, "d", "d2");
            write(wc, "d2/f2", "2\nchanged\n");
            write(wc, "d2/sub/new.txt", "new\n");
            fs::remove_file(wc.join("d2/sub/g")).expect("rm");
        },
        from: "d",
        to: "d2",
        svn_extra: &[],
        theirs: None,
        expect: MarkMove::Moved,
    });
}

#[test]
fn deleted_source_and_added_destination() {
    check(&Case {
        name: "delete-add",
        prep: &|o, wc| {
            fs::copy(wc.join("top.txt"), wc.join("renamed.txt")).expect("copy");
            svn(o, wc, &["rm", "-q", "top.txt"]);
            svn(o, wc, &["add", "-q", "renamed.txt"]);
        },
        from: "top.txt",
        to: "renamed.txt",
        svn_extra: &[],
        theirs: None,
        expect: MarkMove::Moved,
    });
    check(&Case {
        name: "delete-add-dir",
        prep: &|o, wc| {
            rename(wc, "d", "dd");
            write(wc, "dd/extra.txt", "x\n");
            svn(o, wc, &["rm", "-q", "d"]);
            svn(o, wc, &["add", "-q", "dd"]);
        },
        from: "d",
        to: "dd",
        svn_extra: &[],
        theirs: None,
        expect: MarkMove::Moved,
    });
}

#[test]
fn copy_plus_delete_becomes_a_move() {
    check(&Case {
        name: "copy-delete",
        prep: &|o, wc| {
            svn(o, wc, &["cp", "-q", "d", "dcopy"]);
            svn(o, wc, &["rm", "-q", "d"]);
        },
        from: "d",
        to: "dcopy",
        svn_extra: &[],
        // The rows of a plain `svn move d dcopy`.
        theirs: Some(&|o, wc| {
            svn(o, wc, &["revert", "-q", "-R", "d", "dcopy"]);
            fs::remove_dir_all(wc.join("dcopy")).ok();
            svn(o, wc, &["mv", "-q", "d", "dcopy"]);
        }),
        expect: MarkMove::Moved,
    });
}

#[test]
fn mixed_revision_directory_is_a_copy_plus_delete() {
    check(&Case {
        name: "mixed",
        prep: &|o, wc| {
            // r2 changes d/f2 (committed once, from the seed working copy); updating only that
            // file leaves d mixed-revision.
            let seed = wc.parent().expect("parent").join("seed");
            if !read(&seed, "d/f2").contains("newer") {
                write(&seed, "d/f2", "2\nnewer\n");
                svn(o, &seed, &["commit", "-q", "-m", "r2"]);
            }
            svn(o, wc, &["update", "-q", "d/f2"]);
            rename(wc, "d", "dm");
        },
        from: "d",
        to: "dm",
        svn_extra: &["--allow-mixed-revisions"],
        theirs: None,
        expect: MarkMove::CopiedAndDeleted,
    });
}

#[test]
fn already_moved_is_a_no_op() {
    let Some(fx) = fixture() else { return };
    let wc = checkout(&fx.oracle, &fx.base, &fx.url, "wc");
    svn(&fx.oracle, &wc, &["mv", "-q", "top.txt", "t2.txt"]);
    let before = dump_rows(&wc);
    let db = open(&wc);
    let got = local::mark_move(
        &db,
        &CancelToken::new(),
        &wc.join("top.txt"),
        &wc.join("t2.txt"),
    )
    .expect("mark_move");
    assert_eq!(got, MarkMove::AlreadyMoved);
    drop(db);
    assert_eq!(dump_rows(&wc), before);
}

#[test]
fn refusals_change_nothing() {
    let Some(fx) = fixture() else { return };
    let wc = checkout(&fx.oracle, &fx.base, &fx.url, "wc");
    write(&wc, "unv.txt", "u\n");
    fs::create_dir(wc.join("unvdir")).expect("mkdir");
    let before = dump_rows(&wc);
    let db = open(&wc);
    let cancel = CancelToken::new();
    let refused = |from: &str, to: &str| {
        let err =
            local::mark_move(&db, &cancel, &wc.join(from), &wc.join(to)).expect_err("refused");
        assert!(
            matches!(
                err.code,
                ErrorCode::WcInvalidOperation | ErrorCode::PathNotFound
            ),
            "{from}->{to}: {err}"
        );
    };
    refused("top.txt", "unv.txt"); // the source is still on disk
    fs::rename(wc.join("top.txt"), wc.join("away.txt")).expect("rename");
    refused("top.txt", "unvdir"); // a file cannot become a directory
    refused("top.txt", "nothing.txt"); // the destination does not exist
    refused("top.txt", "d/f2"); // the destination is versioned
    refused("unv.txt", "away.txt"); // the source is not versioned
    drop(db);
    assert_eq!(dump_rows(&wc), before);
}

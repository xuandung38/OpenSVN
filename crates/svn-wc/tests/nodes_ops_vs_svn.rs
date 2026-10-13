//! Every working-layer operation is executed twice on identical checkouts: once by svn 1.15.0
//! and once through `WcDb`. The resulting NODES, ACTUAL_NODE and PRISTINE rows must be equal and
//! `svn status --xml -v` must report the same on both copies (so the engine's WC is valid for svn).
#![allow(clippy::expect_used, clippy::panic, clippy::type_complexity)]

mod schema_support;

use std::fs;
use std::path::Path;

use schema_support::*;
use svn_types::{NodeKind, RelPath};
use svn_wc::db::{CommitInfo, WcDb, find_wcroot};

fn rp(s: &str) -> RelPath {
    RelPath::new(s).expect("relpath")
}

fn props(kv: &[(&str, &str)]) -> Vec<(Vec<u8>, Vec<u8>)> {
    kv.iter()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect()
}

fn write(wc: &Path, rel: &str, text: &str) {
    if let Some(parent) = wc.join(rel).parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(wc.join(rel), text).expect("write");
}

fn rm(wc: &Path, rel: &str) {
    let p = wc.join(rel);
    if p.is_dir() {
        fs::remove_dir_all(p)
    } else {
        fs::remove_file(p)
    }
    .expect("rm");
}

fn cp(from: &Path, to: &Path) {
    if from.is_dir() {
        fs::create_dir_all(to).expect("mkdir");
        for e in fs::read_dir(from).expect("readdir") {
            let e = e.expect("entry");
            if e.file_name() == ".svn" {
                continue;
            }
            cp(&e.path(), &to.join(e.file_name()));
        }
    } else {
        fs::copy(from, to).expect("copy");
    }
}

fn mv(wc: &Path, a: &str, b: &str) {
    fs::rename(wc.join(a), wc.join(b)).expect("rename");
}

struct Fixture {
    _tmp: tempfile::TempDir,
    url: String,
    base: std::path::PathBuf,
}

/// Repository with `d/{f1(prop k=v),f2,sub/g}`, `e/` and `top.txt` at r1.
fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canon");
    let url = create_repo(&base);
    let seed = checkout(&base, &url, "seed", &[]);
    write(&seed, "d/f1", "1\n");
    write(&seed, "d/f2", "2\n");
    write(&seed, "d/sub/g", "3\n");
    write(&seed, "top.txt", "t\n");
    fs::create_dir(seed.join("e")).expect("mkdir");
    svn(&seed, &["add", "-q", "d", "e", "top.txt"]);
    svn(&seed, &["propset", "-q", "k", "v", "d/f1"]);
    svn(&seed, &["commit", "-q", "-m", "seed"]);
    Fixture {
        _tmp: tmp,
        url,
        base,
    }
}

/// Runs `svn_cmds` on one checkout and `ours` on another, then compares.
fn check(name: &str, prep: &dyn Fn(&Path), svn_cmds: &[&[&str]], ours: &dyn Fn(&WcDb, &Path)) {
    check_after(&|_| {}, name, prep, svn_cmds, ours);
}

/// Like [`check`], but `history` first commits more revisions (from a scratch checkout) so that
/// `prep` can build mixed-revision working copies with `svn update -r`.
fn check_after(
    history: &dyn Fn(&Path),
    name: &str,
    prep: &dyn Fn(&Path),
    svn_cmds: &[&[&str]],
    ours: &dyn Fn(&WcDb, &Path),
) {
    if skip_without_svn_1_15() {
        return;
    }
    let fx = fixture();
    let scratch = checkout(&fx.base, &fx.url, "history", &[]);
    history(&scratch);
    let a = checkout(&fx.base, &fx.url, "svn-side", &[]);
    let b = checkout(&fx.base, &fx.url, "our-side", &[]);
    prep(&a);
    prep(&b);
    for cmd in svn_cmds {
        svn(&a, cmd);
    }
    let db = WcDb::open(&find_wcroot(&b).expect("root")).expect("open");
    ours(&db, &b);
    drop(db);
    assert_rows_equal(name, &dump_rows(&a), &dump_rows(&b));
    let (sa, sb) = (status_xml(&a), status_xml(&b));
    let (sa, sb) = (sa.replace("svn-side", "X"), sb.replace("our-side", "X"));
    if let Some((l, r)) = sa.lines().zip(sb.lines()).find(|(l, r)| l != r) {
        panic!("{name}: svn status differs\n  svn:  {l}\n  ours: {r}");
    }
    assert_eq!(
        sa.lines().count(),
        sb.lines().count(),
        "{name}: status length differs"
    );
}

#[test]
fn add_file_and_directory_tree() {
    check(
        "add",
        &|wc| {
            write(wc, "new.txt", "n\n");
            fs::create_dir(wc.join("nd")).expect("mkdir");
            write(wc, "nd/q", "q\n");
        },
        &[&["add", "-q", "new.txt"], &["add", "-q", "nd"]],
        &|db, _| {
            db.op_add(&rp("new.txt"), NodeKind::File, None, &[])
                .expect("add");
            db.op_add(&rp("nd"), NodeKind::Dir, None, &[]).expect("add");
            db.op_add(&rp("nd/q"), NodeKind::File, None, &[])
                .expect("add");
        },
    );
}

#[test]
fn delete_file_dir_and_nested() {
    check(
        "delete",
        &|_| {},
        &[&["rm", "-q", "top.txt", "d/f2"], &["rm", "-q", "d"]],
        &|db, wc| {
            rm(wc, "top.txt");
            rm(wc, "d/f2");
            db.op_delete(&rp("top.txt"), &[]).expect("rm");
            db.op_delete(&rp("d/f2"), &[]).expect("rm");
            rm(wc, "d");
            db.op_delete(&rp("d"), &[]).expect("rm");
        },
    );
}

#[test]
fn delete_clears_local_props_but_keeps_changelist() {
    check(
        "delete-props",
        &|_| {},
        &[
            &["propset", "-q", "p", "v", "d/f2"],
            &["changelist", "-q", "cl", "d/f2"],
            &["rm", "-q", "--force", "d/f2"],
        ],
        &|db, wc| {
            db.set_props(&rp("d/f2"), &props(&[("p", "v")]), &[])
                .expect("ps");
            db.set_changelist(&rp("d/f2"), Some("cl")).expect("cl");
            rm(wc, "d/f2");
            db.op_delete(&rp("d/f2"), &[]).expect("rm");
        },
    );
}

#[test]
fn replace_by_add_matches_svn() {
    if skip_without_svn_1_15() {
        return;
    }
    // Same delete+add sequence, svn side adds back after deleting.
    let fx = fixture();
    let a = checkout(&fx.base, &fx.url, "svn-side", &[]);
    let b = checkout(&fx.base, &fx.url, "our-side", &[]);
    svn(&a, &["rm", "-q", "top.txt", "d"]);
    write(&a, "top.txt", "again\n");
    svn(&a, &["add", "-q", "top.txt"]);
    fs::create_dir(a.join("d")).expect("mkdir");
    svn(&a, &["add", "-q", "d"]);
    write(&a, "d/f1", "x\n");
    svn(&a, &["add", "-q", "d/f1"]);
    let db = WcDb::open(&b).expect("open");
    rm(&b, "top.txt");
    rm(&b, "d");
    db.op_delete(&rp("top.txt"), &[]).expect("rm");
    db.op_delete(&rp("d"), &[]).expect("rm");
    write(&b, "top.txt", "again\n");
    db.op_add(&rp("top.txt"), NodeKind::File, None, &[])
        .expect("add");
    fs::create_dir(b.join("d")).expect("mkdir");
    db.op_add(&rp("d"), NodeKind::Dir, None, &[]).expect("add");
    write(&b, "d/f1", "x\n");
    db.op_add(&rp("d/f1"), NodeKind::File, None, &[])
        .expect("add");
    drop(db);
    assert_rows_equal("replace", &dump_rows(&a), &dump_rows(&b));
    assert_eq!(
        status_xml(&a).replace("svn-side", "X"),
        status_xml(&b).replace("our-side", "X")
    );
}

#[test]
fn delete_child_inside_copied_tree() {
    check(
        "delete-in-copy",
        &|_| {},
        &[&["cp", "-q", "d", "dc"], &["rm", "-q", "dc/f1", "dc/sub"]],
        &|db, wc| {
            cp(&wc.join("d"), &wc.join("dc"));
            db.op_copy(&rp("d"), &rp("dc"), &[]).expect("cp");
            rm(wc, "dc/f1");
            db.op_delete(&rp("dc/f1"), &[]).expect("rm");
            rm(wc, "dc/sub");
            db.op_delete(&rp("dc/sub"), &[]).expect("rm");
        },
    );
}

#[test]
fn copy_file_dir_and_modified_props() {
    check(
        "copy",
        &|_| {},
        &[&["cp", "-q", "d/f1", "f1copy"], &["cp", "-q", "d", "dcopy"]],
        &|db, wc| {
            fs::copy(wc.join("d/f1"), wc.join("f1copy")).expect("cp");
            db.op_copy(&rp("d/f1"), &rp("f1copy"), &[]).expect("cp");
            cp(&wc.join("d"), &wc.join("dcopy"));
            db.op_copy(&rp("d"), &rp("dcopy"), &[]).expect("cp");
        },
    );
    check(
        "copy-with-local-props",
        &|_| {},
        &[
            &["propset", "-q", "extra", "1", "d/f1"],
            &["cp", "-q", "d/f1", "f1copy"],
        ],
        &|db, wc| {
            db.set_props(&rp("d/f1"), &props(&[("k", "v"), ("extra", "1")]), &[])
                .expect("ps");
            fs::copy(wc.join("d/f1"), wc.join("f1copy")).expect("cp");
            db.op_copy(&rp("d/f1"), &rp("f1copy"), &[]).expect("cp");
        },
    );
}

#[test]
fn move_file_and_directories() {
    check(
        "move",
        &|_| {},
        &[
            &["mv", "-q", "top.txt", "moved.txt"],
            &["mv", "-q", "d/sub", "d/sub2"],
        ],
        &|db, wc| {
            mv(wc, "top.txt", "moved.txt");
            db.op_move(&rp("top.txt"), &rp("moved.txt"), &[])
                .expect("mv");
            mv(wc, "d/sub", "d/sub2");
            db.op_move(&rp("d/sub"), &rp("d/sub2"), &[]).expect("mv");
        },
    );
    check(
        "move-dir",
        &|_| {},
        &[&["mv", "-q", "d", "dm"]],
        &|db, wc| {
            mv(wc, "d", "dm");
            db.op_move(&rp("d"), &rp("dm"), &[]).expect("mv");
        },
    );
}

#[test]
fn revert_add_delete_copy_replace_and_props() {
    check(
        "revert-add",
        &|wc| write(wc, "new.txt", "n\n"),
        &[&["add", "-q", "new.txt"], &["revert", "-q", "new.txt"]],
        &|db, _| {
            db.op_add(&rp("new.txt"), NodeKind::File, None, &[])
                .expect("add");
            db.op_revert(&rp("new.txt"), false, &[]).expect("revert");
        },
    );
    check(
        "revert-delete",
        &|_| {},
        &[&["rm", "-q", "d"], &["revert", "-q", "-R", "d"]],
        &|db, wc| {
            cp(&wc.join("d"), &wc.join("d.bak"));
            rm(wc, "d");
            db.op_delete(&rp("d"), &[]).expect("rm");
            db.op_revert(&rp("d"), true, &[]).expect("revert");
            mv(wc, "d.bak", "d");
        },
    );
    check(
        "revert-copy",
        &|_| {},
        &[
            &["cp", "-q", "d", "dcopy"],
            &["revert", "-q", "-R", "dcopy"],
        ],
        &|db, wc| {
            cp(&wc.join("d"), &wc.join("dcopy"));
            db.op_copy(&rp("d"), &rp("dcopy"), &[]).expect("cp");
            db.op_revert(&rp("dcopy"), true, &[]).expect("revert");
            // svn removes the unmodified copied files when reverting a copy.
            rm(wc, "dcopy");
        },
    );
    check(
        "revert-props-keeps-changelist",
        &|_| {},
        &[
            &["propset", "-q", "p", "v", "top.txt"],
            &["changelist", "-q", "cl", "top.txt"],
            &["revert", "-q", "top.txt"],
        ],
        &|db, _| {
            db.set_props(&rp("top.txt"), &props(&[("p", "v")]), &[])
                .expect("ps");
            db.set_changelist(&rp("top.txt"), Some("cl")).expect("cl");
            db.op_revert(&rp("top.txt"), false, &[]).expect("revert");
        },
    );
}

#[test]
fn revert_moves_from_either_side() {
    check(
        "revert-move-source",
        &|_| {},
        &[
            &["mv", "-q", "top.txt", "moved.txt"],
            &["revert", "-q", "top.txt"],
        ],
        &|db, wc| {
            mv(wc, "top.txt", "moved.txt");
            db.op_move(&rp("top.txt"), &rp("moved.txt"), &[])
                .expect("mv");
            db.op_revert(&rp("top.txt"), false, &[]).expect("revert");
            write(wc, "top.txt", "t\n");
        },
    );
    check(
        "revert-move-dest",
        &|_| {},
        &[
            &["mv", "-q", "top.txt", "moved.txt"],
            &["revert", "-q", "moved.txt"],
        ],
        &|db, wc| {
            mv(wc, "top.txt", "moved.txt");
            db.op_move(&rp("top.txt"), &rp("moved.txt"), &[])
                .expect("mv");
            db.op_revert(&rp("moved.txt"), false, &[]).expect("revert");
            rm(wc, "moved.txt");
            write(wc, "top.txt", "t\n");
        },
    );
}

#[test]
fn props_and_changelists() {
    check(
        "props",
        &|_| {},
        &[
            &["propset", "-q", "a", "1", "d/f1"],
            &["propset", "-q", "b", "2", "e"],
            &["propdel", "-q", "k", "d/f1"],
            &["changelist", "-q", "one", "d/f2", "top.txt"],
            &["changelist", "-q", "--remove", "top.txt"],
        ],
        &|db, _| {
            db.set_props(&rp("d/f1"), &props(&[("k", "v"), ("a", "1")]), &[])
                .expect("ps");
            db.set_props(&rp("e"), &props(&[("b", "2")]), &[])
                .expect("ps");
            db.set_props(&rp("d/f1"), &props(&[("a", "1")]), &[])
                .expect("ps");
            db.set_changelist(&rp("d/f2"), Some("one")).expect("cl");
            db.set_changelist(&rp("top.txt"), Some("one")).expect("cl");
            db.set_changelist(&rp("top.txt"), None).expect("cl");
        },
    );
    // Setting props back to the pristine set removes the ACTUAL row.
    check(
        "props-back-to-pristine",
        &|_| {},
        &[
            &["propset", "-q", "a", "1", "d/f1"],
            &["propdel", "-q", "a", "d/f1"],
        ],
        &|db, _| {
            db.set_props(&rp("d/f1"), &props(&[("k", "v"), ("a", "1")]), &[])
                .expect("ps");
            db.set_props(&rp("d/f1"), &props(&[("k", "v")]), &[])
                .expect("ps");
        },
    );
}

#[test]
fn commit_folds_changes_into_base_like_svn() {
    if skip_without_svn_1_15() {
        return;
    }
    let fx = fixture();
    let a = checkout(&fx.base, &fx.url, "svn-side", &[]);
    let b = checkout(&fx.base, &fx.url, "our-side", &[]);
    // Same local changes on both sides.
    for wc in [&a, &b] {
        write(wc, "n.txt", "new\n");
    }
    svn(&a, &["add", "-q", "n.txt"]);
    svn(&a, &["propset", "-q", "a", "b", "n.txt"]);
    svn(&a, &["cp", "-q", "d", "dc"]);
    svn(&a, &["rm", "-q", "top.txt"]);
    svn(&a, &["propset", "-q", "z", "y", "d/f2"]);
    svn(&a, &["changelist", "-q", "cl", "d/f2"]);
    {
        let db = WcDb::open(&b).expect("open");
        db.op_add(&rp("n.txt"), NodeKind::File, None, &[])
            .expect("add");
        db.set_props(&rp("n.txt"), &props(&[("a", "b")]), &[])
            .expect("ps");
        cp(&b.join("d"), &b.join("dc"));
        db.op_copy(&rp("d"), &rp("dc"), &[]).expect("cp");
        rm(&b, "top.txt");
        db.op_delete(&rp("top.txt"), &[]).expect("rm");
        db.set_props(&rp("d/f2"), &props(&[("z", "y")]), &[])
            .expect("ps");
        db.set_changelist(&rp("d/f2"), Some("cl")).expect("cl");
    }
    svn(&a, &["commit", "-q", "-m", "c"]);

    // Facts the committing client learns from the server / the new file text.
    let conn = open_ro(&a);
    let (date, author, n_ck): (i64, String, String) = conn
        .query_row(
            "SELECT changed_date, changed_author, checksum FROM nodes WHERE local_relpath = 'n.txt'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("n.txt base");
    let pristine: Vec<(String, i64, String)> = conn
        .prepare("SELECT checksum, size, md5_checksum FROM pristine")
        .expect("p")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("q")
        .map(|r| r.expect("row"))
        .collect();
    drop(conn);

    let db = WcDb::open(&b).expect("open");
    db.with_write(|tx| {
        for (ck, size, md5) in &pristine {
            tx.execute(
                "INSERT OR IGNORE INTO pristine (checksum, size, refcount, md5_checksum) VALUES (?1, ?2, 0, ?3)",
                (ck, size, md5),
            )
            .map_err(svn_wc::db::sql_err)?;
        }
        Ok(())
    })
    .expect("pristine rows");
    let info = |checksum: Option<&str>| CommitInfo {
        new_rev: 2,
        date: Some(date),
        author: Some(author.clone()),
        checksum: checksum.map(str::to_owned),
        ..CommitInfo::default()
    };
    db.commit_node(&rp("n.txt"), &info(Some(&n_ck)), &[])
        .expect("commit n");
    db.commit_node(&rp("dc"), &info(None), &[])
        .expect("commit dc");
    db.commit_node(&rp("top.txt"), &info(None), &[])
        .expect("commit top");
    db.commit_node(&rp("d/f2"), &info(None), &[])
        .expect("commit f2");
    drop(db);
    let norm = |rows: Vec<String>| {
        rows.into_iter()
            .map(|r| r.replace("ts=true", "ts=false"))
            .collect::<Vec<_>>()
    };
    assert_rows_equal("commit", &norm(dump_rows(&a)), &norm(dump_rows(&b)));
}

#[test]
fn copy_carries_deleted_children_of_the_source() {
    check(
        "copy-with-deleted-child",
        &|_| {},
        &[&["rm", "-q", "d/f2"], &["cp", "-q", "d", "dc"]],
        &|db, wc| {
            rm(wc, "d/f2");
            db.op_delete(&rp("d/f2"), &[]).expect("rm");
            cp(&wc.join("d"), &wc.join("dc"));
            db.op_copy(&rp("d"), &rp("dc"), &[]).expect("cp");
        },
    );
    // Copy of a copy keeps the delete layers of its own deleted child as well.
    check(
        "copy-of-copy-with-deleted-child",
        &|_| {},
        &[
            &["cp", "-q", "d", "dc"],
            &["rm", "-q", "dc/f1"],
            &["cp", "-q", "dc", "dc2"],
        ],
        &|db, wc| {
            cp(&wc.join("d"), &wc.join("dc"));
            db.op_copy(&rp("d"), &rp("dc"), &[]).expect("cp");
            rm(wc, "dc/f1");
            db.op_delete(&rp("dc/f1"), &[]).expect("rm");
            cp(&wc.join("dc"), &wc.join("dc2"));
            db.op_copy(&rp("dc"), &rp("dc2"), &[]).expect("cp");
        },
    );
}

#[test]
fn recursive_revert_removes_working_layers_of_descendants() {
    check(
        "revert-R-descendant-add",
        &|wc| write(wc, "d/new.txt", "n\n"),
        &[
            &["add", "-q", "d/new.txt"],
            &["rm", "-q", "d/f2"],
            &["revert", "-q", "-R", "d"],
        ],
        &|db, wc| {
            db.op_add(&rp("d/new.txt"), NodeKind::File, None, &[])
                .expect("add");
            rm(wc, "d/f2");
            db.op_delete(&rp("d/f2"), &[]).expect("rm");
            db.op_revert(&rp("d"), true, &[]).expect("revert -R");
            write(wc, "d/f2", "2\n");
        },
    );
}

#[test]
fn non_recursive_revert_of_an_operation_root_with_child_operations_is_refused() {
    if skip_without_svn_1_15() {
        return;
    }
    let fx = fixture();
    let a = checkout(&fx.base, &fx.url, "svn-side", &[]);
    let b = checkout(&fx.base, &fx.url, "our-side", &[]);
    svn(&a, &["cp", "-q", "d", "dc"]);
    svn(&a, &["rm", "-q", "dc/f1"]);
    let refused = std::process::Command::new("svn")
        .args(["revert", "dc"])
        .current_dir(&a)
        .output()
        .expect("svn");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("E155038"));

    let db = WcDb::open(&b).expect("open");
    cp(&b.join("d"), &b.join("dc"));
    db.op_copy(&rp("d"), &rp("dc"), &[]).expect("cp");
    rm(&b, "dc/f1");
    db.op_delete(&rp("dc/f1"), &[]).expect("rm");
    let err = db.op_revert(&rp("dc"), false, &[]).expect_err("refused");
    assert_eq!(err.svn_code, Some(155_038));
    drop(db);
    assert_rows_equal(
        "refused revert changes nothing",
        &dump_rows(&a),
        &dump_rows(&b),
    );
}

#[test]
fn moving_a_local_copy_is_a_plain_copy_and_children_of_copies_move_like_svn() {
    check(
        "move-of-copy",
        &|_| {},
        &[
            &["cp", "-q", "top.txt", "c.txt"],
            &["mv", "-q", "c.txt", "m.txt"],
        ],
        &|db, wc| {
            fs::copy(wc.join("top.txt"), wc.join("c.txt")).expect("cp");
            db.op_copy(&rp("top.txt"), &rp("c.txt"), &[]).expect("cp");
            mv(wc, "c.txt", "m.txt");
            db.op_move(&rp("c.txt"), &rp("m.txt"), &[]).expect("mv");
        },
    );
    check(
        "move-child-of-copy",
        &|_| {},
        &[&["cp", "-q", "d", "dc"], &["mv", "-q", "dc/f1", "dc/f1b"]],
        &|db, wc| {
            cp(&wc.join("d"), &wc.join("dc"));
            db.op_copy(&rp("d"), &rp("dc"), &[]).expect("cp");
            mv(wc, "dc/f1", "dc/f1b");
            db.op_move(&rp("dc/f1"), &rp("dc/f1b"), &[]).expect("mv");
        },
    );
}

#[test]
fn commit_of_a_replaced_directory_drops_the_old_base_tree() {
    if skip_without_svn_1_15() {
        return;
    }
    let fx = fixture();
    let a = checkout(&fx.base, &fx.url, "svn-side", &[]);
    let b = checkout(&fx.base, &fx.url, "our-side", &[]);
    svn(&a, &["rm", "-q", "d"]);
    fs::create_dir(a.join("d")).expect("mkdir");
    svn(&a, &["add", "-q", "d"]);
    {
        let db = WcDb::open(&b).expect("open");
        rm(&b, "d");
        db.op_delete(&rp("d"), &[]).expect("rm");
        fs::create_dir(b.join("d")).expect("mkdir");
        db.op_add(&rp("d"), NodeKind::Dir, None, &[]).expect("add");
    }
    svn(&a, &["commit", "-q", "-m", "replace"]);
    let (date, author): (i64, String) = open_ro(&a)
        .query_row(
            "SELECT changed_date, changed_author FROM nodes WHERE local_relpath = 'd'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("d base");
    let db = WcDb::open(&b).expect("open");
    let info = CommitInfo {
        new_rev: 2,
        date: Some(date),
        author: Some(author),
        ..CommitInfo::default()
    };
    db.commit_nodes(&[(rp("d"), info)], &[]).expect("commit");
    drop(db);
    assert_rows_equal("commit-replace", &dump_rows(&a), &dump_rows(&b));
}

#[test]
fn commit_of_a_replacement_keeps_the_delete_layers_of_the_new_copy() {
    if skip_without_svn_1_15() {
        return;
    }
    let fx = fixture();
    let a = checkout(&fx.base, &fx.url, "svn-side", &[]);
    let b = checkout(&fx.base, &fx.url, "our-side", &[]);
    // `e` exists in the repository: replace it by a copy of `d`, then delete a child of the copy.
    svn(&a, &["cp", "-q", "d", "e"]);
    svn(&a, &["commit", "-q", "-m", "add e"]);
    svn(&a, &["update", "-q"]);
    svn(&b, &["update", "-q"]);
    svn(&a, &["rm", "-q", "e"]);
    svn(&a, &["cp", "-q", "d", "e"]);
    svn(&a, &["rm", "-q", "e/f2"]);
    {
        let db = WcDb::open(&b).expect("open");
        rm(&b, "e");
        db.op_delete(&rp("e"), &[]).expect("rm e");
        cp(&b.join("d"), &b.join("e"));
        db.op_copy(&rp("d"), &rp("e"), &[]).expect("cp");
        rm(&b, "e/f2");
        db.op_delete(&rp("e/f2"), &[]).expect("rm f2");
    }
    svn(&a, &["commit", "-q", "-m", "replace e"]);
    let (date, author): (i64, String) = open_ro(&a)
        .query_row(
            "SELECT changed_date, changed_author FROM nodes WHERE local_relpath = 'e' AND op_depth = 0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("e base");
    let db = WcDb::open(&b).expect("open");
    let info = CommitInfo {
        new_rev: 3,
        date: Some(date),
        author: Some(author),
        ..CommitInfo::default()
    };
    db.commit_nodes(&[(rp("e"), info)], &[]).expect("commit");
    drop(db);
    assert_rows_equal(
        "commit-replace-keeps-deletes",
        &dump_rows(&a),
        &dump_rows(&b),
    );
}

/// Two more revisions: r2 changes `d/f2` and `d/sub/g`, r3 deletes `d/f1`.
fn more_history(wc: &Path) {
    write(wc, "d/f2", "two\n");
    write(wc, "d/sub/g", "three\n");
    svn(wc, &["commit", "-q", "-m", "r2"]);
    svn(wc, &["rm", "-q", "d/f1"]);
    svn(wc, &["commit", "-q", "-m", "r3"]);
}

fn copy_d_to_dc(db: &WcDb, wc: &Path) {
    cp(&wc.join("d"), &wc.join("dc"));
    db.op_copy(&rp("d"), &rp("dc"), &[]).expect("cp");
}

#[test]
fn commit_nodes_accepts_the_child_items_svn_gathers_for_a_replaced_root() {
    if skip_without_svn_1_15() {
        return;
    }
    let fx = fixture();
    let a = checkout(&fx.base, &fx.url, "svn-side", &[]);
    let b = checkout(&fx.base, &fx.url, "our-side", &[]);
    // `e` exists in the repository: replace it by a copy of `d`, then delete a child of the copy.
    svn(&a, &["cp", "-q", "d", "e"]);
    svn(&a, &["commit", "-q", "-m", "add e"]);
    svn(&a, &["update", "-q"]);
    svn(&b, &["update", "-q"]);
    svn(&a, &["rm", "-q", "e"]);
    svn(&a, &["cp", "-q", "d", "e"]);
    svn(&a, &["rm", "-q", "e/f2"]);
    {
        let db = WcDb::open(&b).expect("open");
        rm(&b, "e");
        db.op_delete(&rp("e"), &[]).expect("rm e");
        cp(&b.join("d"), &b.join("e"));
        db.op_copy(&rp("d"), &rp("e"), &[]).expect("cp");
        rm(&b, "e/f2");
        db.op_delete(&rp("e/f2"), &[]).expect("rm f2");
    }
    svn(&a, &["commit", "-q", "-m", "replace e"]);
    let (date, author): (i64, String) = open_ro(&a)
        .query_row(
            "SELECT changed_date, changed_author FROM nodes WHERE local_relpath = 'e' AND op_depth = 0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("e base");
    let db = WcDb::open(&b).expect("open");
    let info = CommitInfo {
        new_rev: 3,
        date: Some(date),
        author: Some(author),
        ..CommitInfo::default()
    };
    db.commit_nodes(&[(rp("e"), info.clone()), (rp("e/f2"), info)], &[])
        .expect("commit");
    drop(db);
    assert_rows_equal(
        "commit-replace-with-child-items",
        &dump_rows(&a),
        &dump_rows(&b),
    );
}

#[test]
fn copy_of_a_mixed_revision_directory_layers_like_svn() {
    check_after(
        &more_history,
        "copy-mixed",
        &|wc| svn(wc, &["update", "-q", "-r1", "d/sub"]),
        &[&["cp", "-q", "d", "dc"]],
        &copy_d_to_dc,
    );
}

#[test]
fn copy_of_nested_mixed_revisions_layers_each_level() {
    check_after(
        &more_history,
        "copy-mixed-nested",
        &|wc| {
            svn(wc, &["update", "-q", "-r1", "d/sub"]);
            svn(wc, &["update", "-q", "-r3", "d/sub/g"]);
        },
        &[&["cp", "-q", "d", "dc"]],
        &copy_d_to_dc,
    );
}

#[test]
fn copy_keeps_not_present_children_at_their_revision() {
    check_after(
        &more_history,
        "copy-not-present",
        &|wc| {
            svn(wc, &["update", "-q", "-r1"]);
            svn(wc, &["update", "-q", "-r3", "d/f1"]);
        },
        &[&["cp", "-q", "d", "dc"]],
        &copy_d_to_dc,
    );
}

#[test]
fn copy_keeps_excluded_children_excluded() {
    check(
        "copy-excluded",
        &|wc| svn(wc, &["update", "-q", "--set-depth", "exclude", "d/sub"]),
        &[&["cp", "-q", "d", "dc"]],
        &|db, wc| {
            fs::create_dir_all(wc.join("dc")).expect("mkdir");
            cp(&wc.join("d"), &wc.join("dc"));
            db.op_copy(&rp("d"), &rp("dc"), &[]).expect("cp");
        },
    );
}

#[test]
fn copy_of_a_single_mixed_revision_file_needs_no_layering() {
    // A copy of a single mixed-revision child must not need layering: it is its own root.
    check_after(
        &more_history,
        "copy-mixed-file",
        &|wc| svn(wc, &["update", "-q", "-r1", "d/sub/g"]),
        &[&["cp", "-q", "d/sub/g", "gg"]],
        &|db, wc| {
            fs::copy(wc.join("d/sub/g"), wc.join("gg")).expect("copy");
            db.op_copy(&rp("d/sub/g"), &rp("gg"), &[]).expect("cp");
        },
    );
}

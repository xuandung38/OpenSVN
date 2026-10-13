//! Every local operation runs once through svn 1.15 on one checkout and once through
//! `svn-client` on another; the wc.db rows and `svn status` of both must be identical, and the
//! files on disk must match.
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use support::*;
use svn_client::local::{
    self, AddConfig, AddOptions, DeleteOptions, ResolveChoice, ResolveScope, RevertOptions,
};
use svn_types::{CancelToken, Depth};
use svn_wc::WcDb;

/// Lists files below `wc` (excluding `.svn`) with their content for comparison.
fn tree(wc: &Path) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = fs::read_dir(dir).expect("rd").flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
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
                    fs::read_to_string(e.path()).unwrap_or_default()
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(wc, wc, &mut out);
    out
}

struct Case<'a> {
    name: &'a str,
    prep: &'a dyn Fn(&Path),
    svn_cmds: &'a [&'a [&'a str]],
    ours: &'a dyn Fn(&WcDb, &CancelToken, &Path),
}

fn check(c: &Case<'_>) {
    let Some(fx) = fixture() else { return };
    let a = checkout(&fx.oracle, &fx.base, &fx.url, "svn-side");
    let b = checkout(&fx.oracle, &fx.base, &fx.url, "our-side");
    (c.prep)(&a);
    (c.prep)(&b);
    for cmd in c.svn_cmds {
        svn(&fx.oracle, &a, cmd);
    }
    let db = open(&b);
    let cancel = CancelToken::new();
    (c.ours)(&db, &cancel, &b);
    assert!(db.work_items().expect("wq").is_empty(), "{}: queue", c.name);
    drop(db);
    assert_rows_equal(c.name, &dump_rows(&a), &dump_rows(&b));
    let (sa, sb) = (
        status_xml(&fx.oracle, &a).replace("svn-side", "X"),
        status_xml(&fx.oracle, &b).replace("our-side", "X"),
    );
    assert_eq!(sa, sb, "{}: svn status differs", c.name);
    assert_eq!(tree(&a), tree(&b), "{}: files on disk differ", c.name);
}

fn p(wc: &Path, rel: &str) -> PathBuf {
    wc.join(rel)
}

fn cfg() -> AddConfig {
    AddConfig::default()
}

#[test]
fn add_file_and_tree() {
    check(&Case {
        name: "add",
        prep: &|wc| {
            write(wc, "new.txt", "n\n");
            write(wc, "nd/q", "q\n");
            write(wc, "nd/deep/r", "r\n");
        },
        svn_cmds: &[&["add", "-q", "new.txt"], &["add", "-q", "nd"]],
        ours: &|db, cancel, wc| {
            local::add(
                db,
                cancel,
                &[p(wc, "new.txt"), p(wc, "nd")],
                &AddOptions::default(),
                &cfg(),
            )
            .expect("add");
        },
    });
}

#[test]
fn add_depth_empty_and_ignored_children() {
    check(&Case {
        name: "add-empty",
        prep: &|wc| {
            write(wc, "nd/q", "q\n");
        },
        svn_cmds: &[&["add", "-q", "--depth", "empty", "nd"]],
        ours: &|db, cancel, wc| {
            local::add(
                db,
                cancel,
                &[p(wc, "nd")],
                &AddOptions {
                    depth: Depth::Empty,
                    ..AddOptions::default()
                },
                &cfg(),
            )
            .expect("add");
        },
    });
    check(&Case {
        name: "add-ignored",
        prep: &|wc| {
            write(wc, "nd/keep.txt", "k\n");
            write(wc, "nd/skip.o", "o\n");
            write(wc, "nd/.DS_Store", "x\n");
        },
        svn_cmds: &[&["add", "-q", "nd"]],
        ours: &|db, cancel, wc| {
            local::add(db, cancel, &[p(wc, "nd")], &AddOptions::default(), &cfg()).expect("add");
        },
    });
    check(&Case {
        name: "add-no-ignore",
        prep: &|wc| {
            write(wc, "nd/keep.txt", "k\n");
            write(wc, "nd/skip.o", "o\n");
        },
        svn_cmds: &[&["add", "-q", "--no-ignore", "nd"]],
        ours: &|db, cancel, wc| {
            local::add(
                db,
                cancel,
                &[p(wc, "nd")],
                &AddOptions {
                    no_ignore: true,
                    ..AddOptions::default()
                },
                &cfg(),
            )
            .expect("add");
        },
    });
}

#[test]
fn add_force_into_versioned_dir() {
    check(&Case {
        name: "add-force",
        prep: &|wc| {
            write(wc, "d/new1", "1\n");
            write(wc, "d/sub/new2", "2\n");
        },
        svn_cmds: &[&["add", "-q", "--force", "d"]],
        ours: &|db, cancel, wc| {
            local::add(
                db,
                cancel,
                &[p(wc, "d")],
                &AddOptions {
                    force: true,
                    ..AddOptions::default()
                },
                &cfg(),
            )
            .expect("add");
        },
    });
}

#[test]
fn add_applies_auto_props_and_detects_binary() {
    let config = "[miscellany]\nenable-auto-props = yes\n[auto-props]\n*.txt = svn:eol-style=native;svn:keywords=Id\n*.sh = svn:executable\n";
    check(&Case {
        name: "autoprops",
        prep: &|wc| {
            write(wc, "a.txt", "a\n");
            write(wc, "run.sh", "#!/bin/sh\n");
            fs::write(wc.join("blob.bin"), [0u8, 1, 2, 3, 255, 0]).expect("bin");
            write(wc, "plain.dat", "plain text\n");
        },
        svn_cmds: &[&[
            "add",
            "-q",
            "--config-option",
            "config:miscellany:enable-auto-props=yes",
            "--config-option",
            "config:auto-props:*.txt=svn:eol-style=native;svn:keywords=Id",
            "--config-option",
            "config:auto-props:*.sh=svn:executable",
            "a.txt",
            "run.sh",
            "blob.bin",
            "plain.dat",
        ]],
        ours: &|db, cancel, wc| {
            local::add(
                db,
                cancel,
                &[
                    p(wc, "a.txt"),
                    p(wc, "run.sh"),
                    p(wc, "blob.bin"),
                    p(wc, "plain.dat"),
                ],
                &AddOptions::default(),
                &AddConfig::from_config_text(config),
            )
            .expect("add");
        },
    });
}

#[test]
fn add_inherits_svn_auto_props_property() {
    check(&Case {
        name: "inherited-autoprops",
        prep: &|wc| {
            write(wc, "e/x.txt", "x\n");
        },
        svn_cmds: &[
            &[
                "propset",
                "-q",
                "svn:auto-props",
                "*.txt = svn:eol-style=LF",
                "e",
            ],
            &["add", "-q", "e/x.txt"],
        ],
        ours: &|db, cancel, wc| {
            local::propset(
                db,
                cancel,
                &rp("e"),
                "svn:auto-props",
                Some(b"*.txt = svn:eol-style=LF"),
                local::PropSetOptions::default(),
            )
            .expect("ps");
            local::add(
                db,
                cancel,
                &[p(wc, "e/x.txt")],
                &AddOptions::default(),
                &cfg(),
            )
            .expect("add");
        },
    });
}

#[test]
fn delete_file_dir_and_keep_local() {
    check(&Case {
        name: "delete",
        prep: &|_| {},
        svn_cmds: &[&["rm", "-q", "top.txt", "d/f2"], &["rm", "-q", "d/sub"]],
        ours: &|db, cancel, wc| {
            local::delete(
                db,
                cancel,
                &[p(wc, "top.txt"), p(wc, "d/f2"), p(wc, "d/sub")],
                &DeleteOptions::default(),
            )
            .expect("rm");
        },
    });
    check(&Case {
        name: "delete-keep-local",
        prep: &|_| {},
        svn_cmds: &[&["rm", "-q", "--keep-local", "d"]],
        ours: &|db, cancel, wc| {
            local::delete(
                db,
                cancel,
                &[p(wc, "d")],
                &DeleteOptions {
                    keep_local: true,
                    force: false,
                },
            )
            .expect("rm");
        },
    });
}

#[test]
fn delete_refuses_modified_without_force() {
    let Some(fx) = fixture() else { return };
    let wc = checkout(&fx.oracle, &fx.base, &fx.url, "wc");
    write(&wc, "top.txt", "changed\n");
    write(&wc, "e/unversioned", "u\n");
    let db = open(&wc);
    let cancel = CancelToken::new();
    for target in ["top.txt", "e"] {
        let err = local::delete(&db, &cancel, &[wc.join(target)], &DeleteOptions::default())
            .expect_err("refused");
        assert_eq!(err.code, svn_types::ErrorCode::WcInvalidOperation, "{err}");
    }
    assert!(wc.join("top.txt").exists());
    local::delete(
        &db,
        &cancel,
        &[wc.join("top.txt"), wc.join("e")],
        &DeleteOptions {
            keep_local: false,
            force: true,
        },
    )
    .expect("forced");
    assert!(!wc.join("top.txt").exists());
    assert!(!wc.join("e").exists());
}

#[test]
fn move_and_rename() {
    check(&Case {
        name: "move",
        prep: &|_| {},
        svn_cmds: &[
            &["mv", "-q", "top.txt", "moved.txt"],
            &["mv", "-q", "d/sub", "d/sub2"],
        ],
        ours: &|db, cancel, wc| {
            local::move_path(db, cancel, &p(wc, "top.txt"), &p(wc, "moved.txt")).expect("mv");
            local::move_path(db, cancel, &p(wc, "d/sub"), &p(wc, "d/sub2")).expect("mv");
        },
    });
    check(&Case {
        name: "move-modified",
        prep: &|wc| write(wc, "d/f2", "edited\n"),
        svn_cmds: &[&["mv", "-q", "d", "dm"]],
        ours: &|db, cancel, wc| {
            local::move_path(db, cancel, &p(wc, "d"), &p(wc, "dm")).expect("mv");
        },
    });
}

#[test]
fn copy_wc_to_wc() {
    check(&Case {
        name: "copy",
        prep: &|_| {},
        svn_cmds: &[&["cp", "-q", "d/f1", "f1copy"], &["cp", "-q", "d", "dcopy"]],
        ours: &|db, cancel, wc| {
            local::copy(db, cancel, &p(wc, "d/f1"), &p(wc, "f1copy")).expect("cp");
            local::copy(db, cancel, &p(wc, "d"), &p(wc, "dcopy")).expect("cp");
        },
    });
    check(&Case {
        name: "copy-modified",
        prep: &|wc| write(wc, "d/f2", "edited\n"),
        svn_cmds: &[
            &["propset", "-q", "extra", "1", "d/f1"],
            &["cp", "-q", "d", "dcopy"],
        ],
        ours: &|db, cancel, wc| {
            local::propset(
                db,
                cancel,
                &rp("d/f1"),
                "extra",
                Some(b"1"),
                local::PropSetOptions::default(),
            )
            .expect("ps");
            local::copy(db, cancel, &p(wc, "d"), &p(wc, "dcopy")).expect("cp");
        },
    });
}

#[test]
fn revert_variants_restore_files() {
    check(&Case {
        name: "revert-modified",
        prep: &|wc| write(wc, "d/f2", "edited\n"),
        svn_cmds: &[&["revert", "-q", "d/f2"]],
        ours: &|db, cancel, wc| {
            local::revert(db, cancel, &[p(wc, "d/f2")], &RevertOptions::default()).expect("rv");
        },
    });
    check(&Case {
        name: "revert-deleted-recursive",
        prep: &|_| {},
        svn_cmds: &[&["rm", "-q", "d"], &["revert", "-q", "-R", "d"]],
        ours: &|db, cancel, wc| {
            local::delete(db, cancel, &[p(wc, "d")], &DeleteOptions::default()).expect("rm");
            assert!(!wc.join("d").exists());
            local::revert(
                db,
                cancel,
                &[p(wc, "d")],
                &RevertOptions { recursive: true },
            )
            .expect("rv");
        },
    });
    check(&Case {
        name: "revert-add",
        prep: &|wc| write(wc, "new.txt", "n\n"),
        svn_cmds: &[&["add", "-q", "new.txt"], &["revert", "-q", "new.txt"]],
        ours: &|db, cancel, wc| {
            local::add(
                db,
                cancel,
                &[p(wc, "new.txt")],
                &AddOptions::default(),
                &cfg(),
            )
            .expect("add");
            local::revert(db, cancel, &[p(wc, "new.txt")], &RevertOptions::default()).expect("rv");
        },
    });
    check(&Case {
        name: "revert-props-and-missing",
        prep: &|wc| fs::remove_file(wc.join("d/f2")).expect("rm"),
        svn_cmds: &[
            &["propset", "-q", "p", "v", "d/f1"],
            &["revert", "-q", "-R", "d"],
        ],
        ours: &|db, cancel, wc| {
            local::propset(
                db,
                cancel,
                &rp("d/f1"),
                "p",
                Some(b"v"),
                local::PropSetOptions::default(),
            )
            .expect("ps");
            local::revert(
                db,
                cancel,
                &[p(wc, "d")],
                &RevertOptions { recursive: true },
            )
            .expect("rv");
        },
    });
    check(&Case {
        name: "revert-move",
        prep: &|_| {},
        svn_cmds: &[
            &["mv", "-q", "top.txt", "m.txt"],
            &["revert", "-q", "-R", "."],
        ],
        ours: &|db, cancel, wc| {
            local::move_path(db, cancel, &p(wc, "top.txt"), &p(wc, "m.txt")).expect("mv");
            local::revert(
                db,
                cancel,
                &[wc.to_path_buf()],
                &RevertOptions { recursive: true },
            )
            .expect("rv");
            let _ = fs::remove_file(wc.join("m.txt"));
        },
    });
}

#[test]
fn propset_and_propdel() {
    check(&Case {
        name: "props",
        prep: &|wc| write(wc, "d/crlf", "a\r\nb\r\n"),
        svn_cmds: &[
            &["propset", "-q", "svn:executable", "yes", "d/f1"],
            &["propset", "-q", "svn:ignore", "*.tmp", "d"],
            &["propset", "-q", "svn:needs-lock", "x", "d/f2"],
            &["propset", "-q", "custom", "value", "top.txt"],
            &["propdel", "-q", "k", "d/f1"],
            &["propset", "-q", "svn:mime-type", "text/plain", "top.txt"],
        ],
        ours: &|db, cancel, _| {
            let ps = |rel: &str, n: &str, v: Option<&[u8]>| {
                local::propset(db, cancel, &rp(rel), n, v, local::PropSetOptions::default())
                    .expect("ps");
            };
            ps("d/f1", "svn:executable", Some(b"yes"));
            ps("d", "svn:ignore", Some(b"*.tmp"));
            ps("d/f2", "svn:needs-lock", Some(b"x"));
            ps("top.txt", "custom", Some(b"value"));
            ps("d/f1", "k", None);
            ps("top.txt", "svn:mime-type", Some(b"text/plain"));
        },
    });
}

#[test]
fn propset_validation_errors() {
    let Some(fx) = fixture() else { return };
    let wc = checkout(&fx.oracle, &fx.base, &fx.url, "wc");
    let db = open(&wc);
    let cancel = CancelToken::new();
    let ps = |rel: &str, n: &str, v: &[u8]| {
        local::propset(
            &db,
            &cancel,
            &rp(rel),
            n,
            Some(v),
            local::PropSetOptions::default(),
        )
    };
    assert!(ps("d", "svn:executable", b"*").is_err());
    assert!(ps("top.txt", "svn:ignore", b"x").is_err());
    assert!(ps("top.txt", "bad name", b"x").is_err());
    assert!(ps("top.txt", "svn:eol-style", b"bogus").is_err());
    write(&wc, "top.txt", "a\r\nb\nc\n");
    assert!(ps("top.txt", "svn:eol-style", b"native").is_err());
    assert!(ps("nope", "x", b"y").is_err());
}

#[test]
fn ignore_add_appends_patterns() {
    check(&Case {
        name: "ignore",
        prep: &|_| {},
        svn_cmds: &[&["propset", "-q", "svn:ignore", "*.a\n*.b\n", "d"]],
        ours: &|db, cancel, _| {
            local::ignore_add(db, cancel, &rp("d"), &["*.a".into()]).expect("ign");
            local::ignore_add(db, cancel, &rp("d"), &["*.b".into(), "*.a".into()]).expect("ign");
        },
    });
}

#[test]
fn changelist_membership() {
    check(&Case {
        name: "changelist",
        prep: &|_| {},
        svn_cmds: &[
            &["changelist", "-q", "cl", "d/f1", "top.txt"],
            &["changelist", "-q", "--remove", "top.txt"],
        ],
        ours: &|db, cancel, wc| {
            local::set_changelist(
                db,
                cancel,
                &[p(wc, "d/f1"), p(wc, "top.txt")],
                Some("cl"),
                Depth::Empty,
            )
            .expect("cl");
            local::set_changelist(db, cancel, &[p(wc, "top.txt")], None, Depth::Empty).expect("cl");
        },
    });
    check(&Case {
        name: "changelist-recursive",
        prep: &|_| {},
        svn_cmds: &[&["changelist", "-q", "-R", "all", "d"]],
        ours: &|db, cancel, wc| {
            local::set_changelist(db, cancel, &[p(wc, "d")], Some("all"), Depth::Infinity)
                .expect("cl");
        },
    });
}

#[test]
fn cleanup_runs_leftover_work_items() {
    let Some(fx) = fixture() else { return };
    let wc = checkout(&fx.oracle, &fx.base, &fx.url, "wc");
    let db = open(&wc);
    // An interrupted operation left a file removal in the queue.
    db.queue_work(&[svn_wc::workqueue::WorkItem::FileRemove {
        relpath: "top.txt".into(),
    }
    .to_skel()])
        .expect("queue");
    assert_eq!(db.work_items().expect("wq").len(), 1);
    let cancel = CancelToken::new();
    let report = local::cleanup(&db, &cancel).expect("cleanup");
    assert_eq!(report.work_items, 1);
    assert!(!wc.join("top.txt").exists());
    assert!(db.work_items().expect("wq").is_empty());
}

#[test]
fn operations_finish_leftover_queue_first() {
    let Some(fx) = fixture() else { return };
    let wc = checkout(&fx.oracle, &fx.base, &fx.url, "wc");
    let db = open(&wc);
    db.queue_work(&[svn_wc::workqueue::WorkItem::FileRemove {
        relpath: "top.txt".into(),
    }
    .to_skel()])
        .expect("queue");
    let cancel = CancelToken::new();
    write(&wc, "n.txt", "n\n");
    local::add(
        &db,
        &cancel,
        &[wc.join("n.txt")],
        &AddOptions::default(),
        &cfg(),
    )
    .expect("add");
    assert!(!wc.join("top.txt").exists());
}

/// Builds the same text+property conflict on `svn-side` and `our-side` with svn itself.
fn conflicted_pair(fx: &Fixture) -> (PathBuf, PathBuf) {
    let other = checkout(&fx.oracle, &fx.base, &fx.url, "other");
    write(&other, "top.txt", "theirs\n");
    svn(
        &fx.oracle,
        &other,
        &["propset", "-q", "pp", "theirs", "top.txt"],
    );
    svn(&fx.oracle, &other, &["commit", "-q", "-m", "other"]);
    let mut sides = Vec::new();
    for name in ["svn-side", "our-side"] {
        let wc = checkout(&fx.oracle, &fx.base, &fx.url, name);
        // Check out at r1, then conflict on update to r2.
        svn(&fx.oracle, &wc, &["update", "-q", "-r1"]);
        write(&wc, "top.txt", "mine\n");
        svn(&fx.oracle, &wc, &["propset", "-q", "pp", "mine", "top.txt"]);
        svn(&fx.oracle, &wc, &["update", "-q", "--accept", "postpone"]);
        sides.push(wc);
    }
    (sides.remove(0), sides.remove(0))
}

fn check_resolve(accept: &str, choice: ResolveChoice) {
    let Some(fx) = fixture() else { return };
    let (a, b) = conflicted_pair(&fx);
    assert!(a.join("top.txt.mine").exists(), "svn made a text conflict");
    svn(
        &fx.oracle,
        &a,
        &["resolve", "-q", "--accept", accept, "top.txt"],
    );
    let db = open(&b);
    let cancel = CancelToken::new();
    let done = local::resolve(
        &db,
        &cancel,
        &rp("top.txt"),
        false,
        ResolveScope::ALL,
        choice,
    )
    .expect("resolve");
    assert_eq!(done, vec![rp("top.txt")]);
    assert!(db.work_items().expect("wq").is_empty());
    drop(db);
    assert_rows_equal(accept, &dump_rows(&a), &dump_rows(&b));
    assert_eq!(tree(&a), tree(&b), "{accept}: files differ");
    assert_eq!(
        status_xml(&fx.oracle, &a).replace("svn-side", "X"),
        status_xml(&fx.oracle, &b).replace("our-side", "X")
    );
}

#[test]
fn resolve_working_keeps_the_file() {
    check_resolve("working", ResolveChoice::Working);
}

#[test]
fn resolve_base_theirs_and_mine_full() {
    check_resolve("base", ResolveChoice::Base);
    check_resolve("theirs-full", ResolveChoice::TheirsFull);
    check_resolve("mine-full", ResolveChoice::MineFull);
}

#[test]
fn resolve_nothing_to_do_returns_empty() {
    let Some(fx) = fixture() else { return };
    let wc = checkout(&fx.oracle, &fx.base, &fx.url, "wc");
    let db = open(&wc);
    let cancel = CancelToken::new();
    let done = local::resolve(
        &db,
        &cancel,
        &rp("top.txt"),
        true,
        ResolveScope::ALL,
        ResolveChoice::Working,
    )
    .expect("resolve");
    assert!(done.is_empty());
}

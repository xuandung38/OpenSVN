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
                    ..DeleteOptions::default()
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
            force: true,
            ..DeleteOptions::default()
        },
    )
    .expect("forced");
    assert!(!wc.join("top.txt").exists());
    assert!(!wc.join("e").exists());
}

/// Runs `setup` (svn commands) on two checkouts, then `svn rm target` on one and our delete on
/// the other: svn must refuse with `E<code>`, we must refuse with the same Subversion code,
/// and both working copies must be untouched and identical (rows, status, files on disk).
fn check_refused(name: &str, prep: &dyn Fn(&Path), setup: &[&[&str]], target: &str, code: u32) {
    let Some(fx) = fixture() else { return };
    let a = checkout(&fx.oracle, &fx.base, &fx.url, "svn-side");
    let b = checkout(&fx.oracle, &fx.base, &fx.url, "our-side");
    for wc in [&a, &b] {
        prep(wc);
        for cmd in setup {
            svn(&fx.oracle, wc, cmd);
        }
    }
    let out = fx
        .oracle
        .svn()
        .args(["rm", "-q", target])
        .current_dir(&a)
        .output()
        .expect("svn rm");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{name}: svn accepted the delete");
    assert!(
        stderr.contains(&format!("E{code}")),
        "{name}: svn said {stderr}"
    );

    let db = open(&b);
    let err = local::delete(
        &db,
        &CancelToken::new(),
        &[b.join(target)],
        &DeleteOptions::default(),
    )
    .expect_err(name);
    assert_eq!(err.svn_code, Some(code), "{name}: {err}");
    assert_eq!(err.code, svn_types::ErrorCode::WcInvalidOperation, "{name}");
    assert!(db.work_items().expect("wq").is_empty(), "{name}: queue");
    drop(db);
    assert_rows_equal(name, &dump_rows(&a), &dump_rows(&b));
    assert_eq!(
        status_xml(&fx.oracle, &a).replace("svn-side", "X"),
        status_xml(&fx.oracle, &b).replace("our-side", "X"),
        "{name}: svn status differs"
    );
    assert_eq!(tree(&a), tree(&b), "{name}: files on disk differ");
}

#[test]
fn delete_never_removes_local_work_without_force() {
    // E195006: "Use --force to override this restriction (local modifications may be lost)".
    check_refused(
        "added-file",
        &|wc| write(wc, "new.txt", "precious\n"),
        &[&["add", "-q", "new.txt"]],
        "new.txt",
        195_006,
    );
    check_refused(
        "added-empty-file",
        &|wc| write(wc, "empty.txt", ""),
        &[&["add", "-q", "empty.txt"]],
        "empty.txt",
        195_006,
    );
    check_refused(
        "added-dir-with-files",
        &|wc| write(wc, "nd/a", "precious\n"),
        &[&["add", "-q", "nd"]],
        "nd",
        195_006,
    );
    check_refused(
        "added-dir-with-props",
        &|_| {},
        &[&["mkdir", "-q", "mk"], &["propset", "-q", "p", "v", "mk"]],
        "mk",
        195_006,
    );
    check_refused(
        "copied-then-modified-file",
        &|_| {},
        &[
            &["cp", "-q", "d/f2", "c.txt"],
            &["propset", "-q", "x", "1", "c.txt"],
        ],
        "c.txt",
        195_006,
    );
    check_refused(
        "copied-dir-with-modified-child",
        &|_| {},
        &[
            &["cp", "-q", "d", "dc"],
            &["propset", "-q", "x", "1", "dc/f2"],
        ],
        "dc",
        195_006,
    );
    check_refused(
        "modified-file",
        &|wc| write(wc, "top.txt", "changed\n"),
        &[],
        "top.txt",
        195_006,
    );
    check_refused(
        "modified-file-below-dir",
        &|wc| write(wc, "d/sub/g", "changed\n"),
        &[],
        "d",
        195_006,
    );
    check_refused(
        "replaced-by-plain-add",
        &|_| {},
        &[
            &["rm", "-q", "--keep-local", "top.txt"],
            &["add", "-q", "top.txt"],
        ],
        "top.txt",
        195_006,
    );
    // E200005: unversioned (not ignored) content.
    check_refused(
        "unversioned-child",
        &|wc| write(wc, "e/unv", "u\n"),
        &[],
        "e",
        200_005,
    );
}

#[test]
fn copied_file_is_modified_on_disk_after_copy() {
    // Text change of a copy: svn refuses as well (copied-then-modified).
    let Some(fx) = fixture() else { return };
    let wc = checkout(&fx.oracle, &fx.base, &fx.url, "wc");
    svn(&fx.oracle, &wc, &["cp", "-q", "d/f2", "c.txt"]);
    write(&wc, "c.txt", "mine\n");
    let db = open(&wc);
    let err = local::delete(
        &db,
        &CancelToken::new(),
        &[wc.join("c.txt")],
        &DeleteOptions::default(),
    )
    .expect_err("refused");
    assert_eq!(err.svn_code, Some(195_006), "{err}");
    assert_eq!(read(&wc, "c.txt"), "mine\n");
}

#[test]
fn delete_allows_what_svn_allows() {
    check(&Case {
        name: "rm-unmodified-copy-and-empty-add",
        prep: &|_| {},
        svn_cmds: &[
            &["cp", "-q", "d", "dc"],
            &["cp", "-q", "top.txt", "t.copy"],
            &["mkdir", "-q", "emptydir"],
            &["rm", "-q", "dc", "t.copy", "emptydir"],
        ],
        ours: &|db, cancel, wc| {
            local::copy(db, cancel, &p(wc, "d"), &p(wc, "dc")).expect("cp");
            local::copy(db, cancel, &p(wc, "top.txt"), &p(wc, "t.copy")).expect("cp");
            std::fs::create_dir(p(wc, "emptydir")).expect("mkdir");
            local::add(
                db,
                cancel,
                &[p(wc, "emptydir")],
                &AddOptions::default(),
                &cfg(),
            )
            .expect("add");
            local::delete(
                db,
                cancel,
                &[p(wc, "dc"), p(wc, "t.copy"), p(wc, "emptydir")],
                &DeleteOptions::default(),
            )
            .expect("rm");
        },
    });
    // --keep-local checks nothing: modified and added files stay on disk.
    check(&Case {
        name: "rm-keep-local-modified-and-added",
        prep: &|wc| {
            write(wc, "top.txt", "changed\n");
            write(wc, "new.txt", "precious\n");
        },
        svn_cmds: &[
            &["add", "-q", "new.txt"],
            &["rm", "-q", "--keep-local", "top.txt", "new.txt"],
        ],
        ours: &|db, cancel, wc| {
            local::add(
                db,
                cancel,
                &[p(wc, "new.txt")],
                &AddOptions::default(),
                &cfg(),
            )
            .expect("add");
            local::delete(
                db,
                cancel,
                &[p(wc, "top.txt"), p(wc, "new.txt")],
                &DeleteOptions {
                    keep_local: true,
                    ..DeleteOptions::default()
                },
            )
            .expect("rm");
        },
    });
    // Ignored items do not block and go with their directory.
    check(&Case {
        name: "rm-dir-with-only-ignored-items",
        prep: &|wc| {
            write(wc, "e/x.o", "o\n");
            write(wc, "e/build.o/inner", "i\n");
            write(wc, "d/sub/y.pyc", "p\n");
        },
        svn_cmds: &[&["rm", "-q", "e", "d"]],
        ours: &|db, cancel, wc| {
            local::delete(
                db,
                cancel,
                &[p(wc, "e"), p(wc, "d")],
                &DeleteOptions::default(),
            )
            .expect("rm");
        },
    });
}

#[test]
fn delete_of_a_missing_added_file_is_allowed() {
    // Status '!' is not a local modification for svn's check.
    let Some(fx) = fixture() else { return };
    let a = checkout(&fx.oracle, &fx.base, &fx.url, "svn-side");
    let b = checkout(&fx.oracle, &fx.base, &fx.url, "our-side");
    for wc in [&a, &b] {
        write(wc, "gone.txt", "g\n");
        svn(&fx.oracle, wc, &["add", "-q", "gone.txt"]);
        std::fs::remove_file(wc.join("gone.txt")).expect("rm file");
    }
    svn(&fx.oracle, &a, &["rm", "-q", "gone.txt"]);
    let db = open(&b);
    local::delete(
        &db,
        &CancelToken::new(),
        &[b.join("gone.txt")],
        &DeleteOptions::default(),
    )
    .expect("rm");
    drop(db);
    assert_rows_equal("rm-missing-added", &dump_rows(&a), &dump_rows(&b));
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
fn copy_dir_takes_unversioned_items_along() {
    check(&Case {
        name: "copy-unversioned",
        prep: &|wc| {
            write(wc, "d/unv.txt", "u\n");
            write(wc, "d/x.o", "o\n");
            write(wc, "d/ud/z", "z\n");
            write(wc, "d/sub/u2", "u2\n");
        },
        svn_cmds: &[&["cp", "-q", "d", "dc"]],
        ours: &|db, cancel, wc| {
            local::copy(db, cancel, &p(wc, "d"), &p(wc, "dc")).expect("cp");
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

/// Runs svn with `args` in `cwd`, expecting it to fail; returns its stderr.
fn svn_fails(o: &opensvn_svn_oracle::Oracle, cwd: &Path, args: &[&str]) -> String {
    let out = o.svn().args(args).current_dir(cwd).output().expect("svn");
    assert!(!out.status.success(), "svn {args:?} succeeded");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// svn 1.15 refuses to delete a file external, present or missing, whatever `--force` and
/// `--keep-local` say (E155030, libsvn_client's `check_external`); so do we, with the same
/// message, and neither working copy changes. A commit never deletes one either.
#[test]
fn delete_refuses_a_file_external_like_svn() {
    let Some(fx) = fixture() else { return };
    let seed = checkout(&fx.oracle, &fx.base, &fx.url, "ext-seed");
    svn(
        &fx.oracle,
        &seed,
        &["propset", "-q", "svn:externals", "^/top.txt ext.txt", "e"],
    );
    svn(&fx.oracle, &seed, &["commit", "-q", "-m", "external"]);
    let a = checkout(&fx.oracle, &fx.base, &fx.url, "svn-side");
    let b = checkout(&fx.oracle, &fx.base, &fx.url, "our-side");
    assert_eq!(read(&b, "e/ext.txt"), "t\n");

    for missing in [false, true] {
        if missing {
            fs::remove_file(a.join("e/ext.txt")).expect("rm");
            fs::remove_file(b.join("e/ext.txt")).expect("rm");
        }
        for (flags, keep_local, force) in [
            (&[][..], false, false),
            (&["--keep-local"][..], true, false),
            (&["--force"][..], false, true),
        ] {
            let mut args = vec!["rm", "-q"];
            args.extend_from_slice(flags);
            args.push("e/ext.txt");
            let stderr = svn_fails(&fx.oracle, &a, &args);
            let db = open(&b);
            let err = local::delete(
                &db,
                &CancelToken::new(),
                &[b.join("e/ext.txt")],
                &DeleteOptions {
                    keep_local,
                    force,
                    ..DeleteOptions::default()
                },
            )
            .expect_err("external");
            drop(db);
            assert_eq!(err.svn_code, Some(155_030), "{err}");
            assert_eq!(err.code, svn_types::ErrorCode::WcInvalidOperation);
            assert_eq!(
                stderr.trim().replace("svn-side", "X"),
                format!("svn: E155030: {}", err.message).replace("our-side", "X"),
                "missing={missing} {flags:?}"
            );
        }
        assert_rows_equal("file-external", &dump_rows(&a), &dump_rows(&b));
        assert_eq!(
            status_xml(&fx.oracle, &a).replace("svn-side", "X"),
            status_xml(&fx.oracle, &b).replace("our-side", "X"),
            "missing={missing}: svn status differs"
        );
    }

    // A deletion of the external written by anything else is refused by the commit harvest
    // instead of deleting the external's source (`top.txt`) from the repository.
    let db = open(&b);
    db.with_write(|tx| {
        tx.execute(
            "INSERT INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, presence, kind) \
             SELECT wc_id, local_relpath, 2, parent_relpath, 'base-deleted', kind FROM nodes \
             WHERE local_relpath = 'e/ext.txt' AND op_depth = 0",
            [],
        )
        .map(|_| ())
        .map_err(svn_wc::db::sql_err)
    })
    .expect("plant");
    let err = svn_client::commit::collect::collect(
        &db,
        &[rp("")],
        &svn_client::commit::collect::CollectOptions {
            depth: Depth::Infinity,
            changelists: Vec::new(),
        },
    )
    .expect_err("collect");
    assert_eq!(err.svn_code, Some(155_030), "{err}");
}

/// svn 1.15 `svn add` of an unversioned folder skips a working copy nested in it at any depth
/// with W150002, leaving it (and all it holds) unversioned; we add the same nodes and return the
/// same warning. Each folder holds a single entry: past a nested working copy svn abandons the
/// rest of the folder in hash order, where we keep adding the siblings.
#[test]
fn add_skips_nested_working_copies_like_svn() {
    let Some(fx) = fixture() else { return };
    let a = checkout(&fx.oracle, &fx.base, &fx.url, "svn-side");
    let b = checkout(&fx.oracle, &fx.base, &fx.url, "our-side");
    for (wc, side) in [(&a, "a"), (&b, "b")] {
        for (name, at) in [("n1", "x/nested"), ("n2", "w/y/deeper")] {
            let other = checkout(&fx.oracle, &fx.base, &fx.url, &format!("{side}-{name}"));
            write(&other, "inner-local.txt", "local\n");
            fs::create_dir_all(wc.join(at).parent().expect("parent")).expect("mkdir");
            fs::rename(&other, wc.join(at)).expect("mv");
        }
    }
    let mut svn_warnings = Vec::new();
    for target in ["x", "w"] {
        // Without `-q`: svn only prints the warning through its notifications.
        let stderr = svn_fails(&fx.oracle, &a, &["add", target]);
        let warning = stderr
            .lines()
            .find(|l| l.starts_with("svn: warning: W150002: "))
            .unwrap_or_else(|| panic!("{stderr}"))
            .to_owned();
        svn_warnings.push(warning.replace("svn-side", "X"));
    }

    let db = open(&b);
    let out = local::add_reporting(
        &db,
        &CancelToken::new(),
        &[p(&b, "x"), p(&b, "w")],
        &AddOptions::default(),
        &cfg(),
    )
    .expect("add");
    assert_eq!(
        out.added
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["x", "w", "w/y"]
    );
    let ours: Vec<String> = out
        .warnings
        .iter()
        .map(|w| {
            assert_eq!(w.svn_code, Some(150_002));
            format!("svn: warning: W150002: {}", w.message).replace("our-side", "X")
        })
        .collect();
    assert_eq!(ours, svn_warnings);

    // A nested working copy as an explicit target is refused before anything is added.
    let err = local::add(
        &db,
        &CancelToken::new(),
        &[p(&b, "top-new.txt"), p(&b, "x/nested")],
        &AddOptions::default(),
        &cfg(),
    )
    .expect_err("nested target");
    assert_eq!(err.svn_code, Some(150_002), "{err}");
    drop(db);
    assert_rows_equal("add-nested", &dump_rows(&a), &dump_rows(&b));
    assert_eq!(
        status_xml(&fx.oracle, &a).replace("svn-side", "X"),
        status_xml(&fx.oracle, &b).replace("our-side", "X"),
        "svn status differs"
    );
}

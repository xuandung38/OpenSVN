//! `WcDb::read_repos_info` against `svn info` for plain, copied, added, deleted and replaced nodes.
#![allow(clippy::expect_used, clippy::panic)]

mod schema_support;

use std::fs;
use std::path::Path;

use schema_support::*;
use svn_types::{ErrorCode, NodeKind, RelPath};
use svn_wc::db::WcDb;

fn rp(s: &str) -> RelPath {
    RelPath::new(s).expect("relpath")
}

/// What svn reports for `target`: (root URL, uuid, repository-relative path without `^/`).
fn svn_view(wc: &Path, target: &str) -> (String, String, String) {
    let item = |name: &str| {
        svn_stdout(wc, &["info", "--show-item", name, target])
            .trim()
            .to_string()
    };
    let rel = item("repos-relative-url");
    (
        item("repos-root-url"),
        item("repos-uuid"),
        rel.strip_prefix("^/").unwrap_or(&rel).to_string(),
    )
}

fn assert_like_svn(db: &WcDb, wc: &Path, target: &str) {
    let ours = db.read_repos_info(&rp(target)).expect(target);
    assert_eq!(
        (ours.repos_root_url, ours.repos_uuid, ours.repos_relpath),
        svn_view(wc, target),
        "{target}"
    );
}

#[test]
fn repos_info_matches_svn_info_for_every_kind_of_node() {
    if skip_without_svn() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canon");
    let url = create_repo(&base);
    let seed = checkout(&base, &url, "seed", &[]);
    fs::create_dir_all(seed.join("d/sub")).expect("mkdir");
    fs::write(seed.join("d/f1"), "1\n").expect("write");
    fs::write(seed.join("d/sub/g"), "g\n").expect("write");
    svn(&seed, &["add", "-q", "d"]);
    svn(&seed, &["commit", "-q", "-m", "seed"]);
    // Our side and svn's side are the same working copy: svn only reads it with `info`.
    let wc = checkout(&base, &url, "wc", &[]);
    let db = WcDb::open(&wc).expect("open");

    // Plain BASE nodes and the root.
    for p in ["d", "d/f1", "d/sub/g"] {
        assert_like_svn(&db, &wc, p);
    }
    assert_eq!(db.read_repos_info(&rp("")).expect("root").repos_relpath, "");

    // Copy: the copy source is the location, also for children of the copy.
    svn(&wc, &["cp", "-q", "d", "dc"]);
    db.op_copy(&rp("d"), &rp("dc"), &[]).expect("cp");
    for p in ["dc", "dc/f1", "dc/sub/g"] {
        let ours = db.read_repos_info(&rp(p)).expect(p);
        // svn info reports the future URL of a copy (where it would be committed).
        assert_eq!(
            ours.repos_relpath,
            format!("d{}", &p[2..]),
            "{p}: copy source"
        );
    }

    // Added inside a plain directory and inside a copy: parent location plus name.
    fs::write(wc.join("d/new.txt"), "n\n").expect("write");
    fs::write(wc.join("dc/added.txt"), "a\n").expect("write");
    fs::create_dir_all(wc.join("d/nd/deep")).expect("mkdir");
    svn(&wc, &["add", "-q", "d/new.txt", "dc/added.txt", "d/nd"]);
    db.op_add(&rp("d/new.txt"), NodeKind::File, None, &[])
        .expect("add");
    db.op_add(&rp("dc/added.txt"), NodeKind::File, None, &[])
        .expect("add");
    db.op_add(&rp("d/nd"), NodeKind::Dir, None, &[])
        .expect("add dir");
    db.op_add(&rp("d/nd/deep"), NodeKind::Dir, None, &[])
        .expect("add deep");
    for p in ["d/new.txt", "d/nd", "d/nd/deep"] {
        assert_like_svn(&db, &wc, p);
    }
    assert_eq!(
        db.read_repos_info(&rp("dc/added.txt"))
            .expect("added in copy")
            .repos_relpath,
        "dc/added.txt"
    );

    // Deleted node: the location of what it deletes.
    svn(&wc, &["rm", "-q", "d/f1"]);
    db.op_delete(&rp("d/f1"), &[]).expect("rm");
    assert_like_svn(&db, &wc, "d/f1");

    // Replaced node: parent location plus name, equal to the deleted one.
    fs::write(wc.join("d/f1"), "again\n").expect("write");
    svn(&wc, &["add", "-q", "d/f1"]);
    db.op_add(&rp("d/f1"), NodeKind::File, None, &[])
        .expect("re-add");
    assert_like_svn(&db, &wc, "d/f1");

    let err = db.read_repos_info(&rp("nope")).expect_err("unversioned");
    assert_eq!(err.code, ErrorCode::PathNotFound);
}

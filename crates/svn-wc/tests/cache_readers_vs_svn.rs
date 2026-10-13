//! LOCK cache, EXTERNALS, `dav_cache` and `inherited_props` readers checked against what the real
//! `svn` 1.15 wrote into wc.db (the raw rows are the oracle).
#![allow(clippy::expect_used, clippy::panic)]

mod schema_support;

use std::fs;
use std::process::Command;

use rusqlite::OptionalExtension;
use schema_support::*;
use svn_types::{NodeKind, RelPath};
use svn_wc::db::{Presence, WcDb};

fn rp(s: &str) -> RelPath {
    RelPath::new(s).expect("relpath")
}

fn svn_out(cwd: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("svn")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("svn");
    assert!(
        out.status.success(),
        "svn {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Repository with `top.txt`, `e/inner.txt` and `d/sub/g` at r1; returns (tempdir, base, url).
fn fixture() -> (tempfile::TempDir, std::path::PathBuf, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canon");
    let url = create_repo(&base);
    let seed = checkout(&base, &url, "seed", &[]);
    fs::write(seed.join("top.txt"), "t\n").expect("write");
    fs::create_dir_all(seed.join("e")).expect("mkdir");
    fs::write(seed.join("e/inner.txt"), "i\n").expect("write");
    fs::create_dir_all(seed.join("d/sub")).expect("mkdir");
    fs::write(seed.join("d/sub/g"), "g\n").expect("write");
    svn(&seed, &["add", "-q", "top.txt", "e", "d"]);
    svn(&seed, &["commit", "-q", "-m", "seed"]);
    (tmp, base, url)
}

#[test]
fn lock_cache_matches_svn_info() {
    if skip_without_svn() {
        return;
    }
    let (_tmp, base, url) = fixture();
    let wc = checkout(&base, &url, "wc", &[]);
    let db = WcDb::open(&wc).expect("open");
    assert_eq!(db.lock(&rp("top.txt")).expect("lock"), None);
    svn(&wc, &["lock", "-q", "-m", "editing", "top.txt"]);
    let info = svn_out(&wc, &["info", "top.txt"]);
    let token = info
        .lines()
        .find_map(|l| l.strip_prefix("Lock Token: "))
        .expect("svn info shows a lock token")
        .trim()
        .to_string();
    let lock = db.lock(&rp("top.txt")).expect("lock").expect("locked");
    assert_eq!(lock.token, token);
    assert_eq!(lock.comment.as_deref(), Some("editing"));
    assert!(lock.owner.is_some());
    assert!(lock.date.is_some_and(|d| d > 0));
    // The lock belongs to the repository location, not to a path in the working copy.
    assert_eq!(db.lock(&rp("e")).expect("lock"), None);
    svn(&wc, &["unlock", "-q", "top.txt"]);
    assert_eq!(db.lock(&rp("top.txt")).expect("lock"), None);
}

#[test]
fn externals_match_the_rows_svn_wrote() {
    if skip_without_svn() {
        return;
    }
    let (_tmp, base, url) = fixture();
    let wc = checkout(&base, &url, "wc", &[]);
    let root = url.clone();
    svn(
        &wc,
        &[
            "propset",
            "-q",
            "svn:externals",
            &format!("{root}/e ext-dir\n{root}/top.txt ext-file.txt\n"),
            "d",
        ],
    );
    svn(&wc, &["update", "-q"]);

    let db = WcDb::open(&wc).expect("open");
    let dir_ext = db
        .external(&rp("d/ext-dir"))
        .expect("read")
        .expect("dir external");
    assert_eq!(dir_ext.kind, NodeKind::Dir);
    assert_eq!(dir_ext.presence, Presence::Normal);
    assert_eq!(dir_ext.parent_relpath, rp("d"));
    assert_eq!(dir_ext.def_local_relpath, rp("d"));
    assert_eq!(dir_ext.def_repos_relpath, "e");
    assert_eq!(dir_ext.repos_root, root);

    let defined = db.externals_defined_in(&rp("d")).expect("defined");
    let names: Vec<_> = defined
        .iter()
        .map(|e| e.local_relpath.as_str().to_string())
        .collect();
    assert_eq!(names, ["d/ext-dir", "d/ext-file.txt"]);
    assert_eq!(defined[1].kind, NodeKind::File);

    // Every field equals the raw row svn stored.
    let conn = open_ro(&wc);
    let (uuid, def_rel): (String, String) = conn
        .query_row(
            "SELECT r.uuid, e.def_repos_relpath FROM externals e JOIN repository r ON r.id = e.repos_id \
             WHERE e.local_relpath = 'd/ext-file.txt'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("raw row");
    assert_eq!(defined[1].repos_uuid, uuid);
    assert_eq!(defined[1].def_repos_relpath, def_rel);

    assert_eq!(db.externals_under(&rp("")).expect("under").len(), 2);
    assert_eq!(
        db.externals_under(&rp("d/ext-dir")).expect("under").len(),
        1
    );
    assert!(db.externals_defined_in(&rp("e")).expect("none").is_empty());
    assert_eq!(db.external(&rp("top.txt")).expect("none"), None);
}

#[test]
fn dav_cache_and_inherited_props_are_decoded_from_the_base_row() {
    if skip_without_svn() {
        return;
    }
    let (_tmp, base, url) = fixture();
    // A checkout rooted in a subdirectory caches the properties of the parents above it.
    let seed = base.join("seed");
    svn(&seed, &["propset", "-q", "inh", "from-d", "d"]);
    svn(&seed, &["commit", "-q", "-m", "prop on d"]);
    let sub = checkout(&base, &format!("{url}/d/sub"), "subwc", &[]);
    let raw: Option<Vec<u8>> = open_ro(&sub)
        .query_row(
            "SELECT inherited_props FROM nodes WHERE local_relpath = '' AND op_depth = 0",
            [],
            |r| r.get(0),
        )
        .optional()
        .expect("raw")
        .flatten();
    assert!(
        raw.is_some(),
        "svn caches inherited props for a sub-tree checkout"
    );

    let db = WcDb::open(&sub).expect("open");
    let iprops = db.inherited_props(&rp("")).expect("read").expect("cached");
    let found = iprops
        .iter()
        .find(|(_, props)| props.iter().any(|(k, v)| k == b"inh" && v == b"from-d"))
        .expect("the property of d is inherited");
    assert!(
        found.0.ends_with(b"d"),
        "{:?}",
        String::from_utf8_lossy(&found.0)
    );
    assert_eq!(db.inherited_props(&rp("g")).expect("read"), None);

    // dav_cache is written by the RA layer over http; stand in with a well-formed blob.
    assert_eq!(db.dav_cache(&rp("g")).expect("none"), None);
    drop(db);
    let rw = rusqlite::Connection::open(sub.join(".svn/wc.db")).expect("open rw");
    rw.execute(
        "UPDATE nodes SET dav_cache = ?1 WHERE local_relpath = 'g' AND op_depth = 0",
        [b"(25 svn:wc:ra_dav:version-url 12 /v/1/d/sub/g )".to_vec()],
    )
    .expect("set dav_cache");
    drop(rw);
    let db = WcDb::open(&sub).expect("open");
    let dav = db.dav_cache(&rp("g")).expect("read").expect("present");
    assert_eq!(
        dav,
        vec![(
            b"svn:wc:ra_dav:version-url".to_vec(),
            b"/v/1/d/sub/g".to_vec()
        )]
    );
}

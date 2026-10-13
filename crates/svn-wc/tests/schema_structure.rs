//! The wc.db created by OpenSVN must be structurally identical to the one svn creates:
//! tables, columns, keys, indexes, triggers, views, `sqlite_stat1` rows and header pragmas.
#![allow(clippy::expect_used, clippy::panic)]

mod schema_support;

use schema_support::*;
use svn_types::Depth;
use svn_wc::db::{NewWc, WcDb};

fn new_wc() -> NewWc {
    NewWc {
        repos_root_url: "file:///repo".into(),
        repos_uuid: "00000000-0000-0000-0000-000000000000".into(),
        repos_relpath: "trunk".into(),
        revision: 0,
        depth: Depth::Infinity,
    }
}

#[test]
fn created_schema_matches_svn_checkout() {
    if skip_without_svn() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = create_repo(tmp.path());
    let svn_wc = checkout(tmp.path(), &url, "svn-wc", &[]);
    let ours = tmp.path().join("ours");
    std::fs::create_dir(&ours).expect("mkdir");
    let db = WcDb::create(&ours, &new_wc()).expect("create");
    assert_eq!(db.format(), 31);
    drop(db);

    let want = structure(&open_ro(&svn_wc));
    let got = structure(&open_ro(&ours));
    assert_eq!(
        want.get("pragma:user_version").map(String::as_str),
        Some("31")
    );
    assert!(
        want.contains_key("table:sqlite_stat1"),
        "svn must install stat1"
    );
    assert_same_structure("svn", &want, "opensvn", &got);
}

#[test]
fn created_schema_matches_svn_checkout_compatible_1_15_is_format_32_and_differs() {
    if skip_without_svn() {
        return;
    }
    // Guard: the comparison above is meaningful only because format 32 really differs.
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = create_repo(tmp.path());
    let wc31 = checkout(tmp.path(), &url, "w31", &[]);
    let wc32 = checkout(tmp.path(), &url, "w32", &["--compatible-version=1.15"]);
    let a = structure(&open_ro(&wc31));
    let b = structure(&open_ro(&wc32));
    assert_ne!(a, b);
    assert_eq!(b.get("pragma:user_version").map(String::as_str), Some("32"));
    assert!(b.contains_key("table:SETTINGS"));
}

#[test]
fn created_db_keeps_rollback_journal_and_default_header() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = WcDb::create(tmp.path(), &new_wc()).expect("create");
    drop(db);
    let conn = open_ro(tmp.path());
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .expect("jm");
    assert_eq!(
        mode, "delete",
        "TRUNCATE is per connection; the file must not be WAL"
    );
    let bytes = std::fs::read(tmp.path().join(".svn/wc.db")).expect("read");
    // SQLite header: file format write/read version bytes are 1 (rollback journal), 2 = WAL.
    assert_eq!((bytes[18], bytes[19]), (1, 1));
    for f in ["format", "entries"] {
        assert_eq!(
            std::fs::read(tmp.path().join(".svn").join(f)).expect("f"),
            b"12\n"
        );
    }
    assert!(tmp.path().join(".svn/tmp").is_dir() && tmp.path().join(".svn/pristine").is_dir());
}

/// Opt-in (`OPENSVN_TEST_DOCKER_SVN=1`): builds the reference working copy with svn 1.14.5 from
/// the `alpine:3.22` image and compares structure. Needs a running Docker daemon.
#[test]
fn created_schema_matches_svn_1_14_5_in_docker() {
    if std::env::var_os("OPENSVN_TEST_DOCKER_SVN").is_none() {
        eprintln!("OPENSVN_TEST_DOCKER_SVN not set: skipping the svn 1.14.5 comparison");
        return;
    }
    let tmp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir");
    let work = tmp.path().canonicalize().expect("canonicalize");
    let script = "apk add -q subversion >/dev/null 2>&1 && svn --version --quiet >/work/version \
                  && svnadmin create /work/repo && svn checkout -q file:///work/repo /work/wc \
                  && chmod -R a+rwX /work";
    let status = std::process::Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &format!("{}:/work", work.display()),
            "alpine:3.22",
            "sh",
            "-c",
            script,
        ])
        .status()
        .expect("docker");
    assert!(status.success(), "docker svn 1.14.5 setup failed");
    assert_eq!(
        std::fs::read_to_string(work.join("version"))
            .expect("version")
            .trim(),
        "1.14.5"
    );

    let ours = work.join("ours");
    std::fs::create_dir(&ours).expect("mkdir");
    drop(WcDb::create(&ours, &new_wc()).expect("create"));
    let want = structure(&open_ro(&work.join("wc")));
    assert_eq!(
        want.get("pragma:user_version").map(String::as_str),
        Some("31")
    );
    assert_same_structure("svn-1.14.5", &want, "opensvn", &structure(&open_ro(&ours)));
}

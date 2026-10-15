//! Checks the committed working-copy fixtures (`fixtures/wc/f31` from svn 1.14.3,
//! `fixtures/wc/f31-svn1.15` and `fixtures/wc/f32` from svn 1.15.0):
//! the manifest is complete, every tarball unpacks to a wc.db of the declared format, the state
//! it claims is really recorded in wc.db, no work queue is left behind, and the harness wc.db
//! dumper reads it.
//!
//! Unpacking needs `zstd` and `tar` on PATH. Without them the content checks are skipped with a
//! message, unless `OPENSVN_REQUIRE_FIXTURE_TOOLS=1` (CI), which turns that into a failure.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rusqlite::Connection;
use serde::Deserialize;

#[derive(Deserialize)]
struct Manifest {
    format: i64,
    svn_version: String,
    wc_dir_in_tarball: String,
    states: Vec<State>,
}

#[derive(Deserialize)]
struct State {
    name: String,
    description: String,
    tarball: String,
    status: String,
    sha256: String,
}

fn fixtures() -> PathBuf {
    opensvn_compat::repo_root().join("fixtures/wc")
}

fn manifest(fmt: &str) -> Manifest {
    let text = std::fs::read_to_string(fixtures().join(fmt).join("manifest.json")).unwrap();
    serde_saphyr::from_str(&text).unwrap()
}

fn tools_available() -> bool {
    let ok = |cmd: &str| {
        Command::new(cmd)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    let have = ok("zstd") && ok("tar");
    if !have {
        assert!(
            std::env::var("OPENSVN_REQUIRE_FIXTURE_TOOLS").as_deref() != Ok("1"),
            "zstd/tar missing and OPENSVN_REQUIRE_FIXTURE_TOOLS=1"
        );
        eprintln!("skipping wc fixture content checks: zstd and tar are needed");
    }
    have
}

fn unpack(tarball: &Path, into: &Path) {
    let mut zstd = Command::new("zstd")
        .args(["-dcq"])
        .arg(tarball)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let status = Command::new("tar")
        .args(["-x", "-f", "-"])
        .arg("-C")
        .arg(into)
        .stdin(zstd.stdout.take().unwrap())
        .status()
        .unwrap();
    assert!(zstd.wait().unwrap().success(), "zstd {}", tarball.display());
    assert!(status.success(), "unpack {}", tarball.display());
}

fn count(db: &Connection, sql: &str) -> i64 {
    db.query_row(sql, [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// What wc.db must show for a state (beyond "opens and has the right format").
fn assert_state(fmt: i64, name: &str, db: &Connection) {
    let c = |sql: &str| count(db, sql);
    let conflicts = "SELECT count(*) FROM ACTUAL_NODE WHERE conflict_data IS NOT NULL";
    match name {
        n if n.starts_with("conflict") || n.starts_with("obstructed") => {
            assert!(c(conflicts) >= 1, "{name}: no conflict recorded")
        }
        "locked" | "lock-stolen" => assert_eq!(c("SELECT count(*) FROM LOCK"), 1, "{name}"),
        "moved-file" | "moved-dir" => assert!(
            c("SELECT count(*) FROM NODES WHERE moved_to IS NOT NULL") >= 1,
            "{name}: no move recorded"
        ),
        "sparse-empty" | "sparse-files" | "sparse-immediates" => {
            let depth = name.trim_start_matches("sparse-");
            let root: String = db
                .query_row(
                    "SELECT depth FROM NODES WHERE local_relpath='' AND op_depth=0",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(root, depth, "{name}");
        }
        "sparse-mixed" => {
            let depth = |rel: &str| -> String {
                db.query_row(
                    "SELECT depth FROM NODES WHERE local_relpath=?1 AND op_depth=0",
                    [rel],
                    |r| r.get(0),
                )
                .unwrap()
            };
            assert_eq!(depth(""), "infinity", "{name}");
            assert_eq!(depth("src"), "empty", "{name}");
            assert_eq!(depth("docs"), "files", "{name}");
        }
        "incomplete" => assert!(
            c("SELECT count(*) FROM NODES WHERE presence='incomplete'") >= 1,
            "{name}: no incomplete node"
        ),
        "server-excluded" => assert!(
            c("SELECT count(*) FROM NODES WHERE presence='server-excluded'") >= 1,
            "{name}: no server-excluded node"
        ),
        "switched-file" => assert_eq!(
            c(
                "SELECT count(*) FROM NODES WHERE op_depth=0 AND local_relpath='README.txt' AND repos_path='branches/feature-x/README.txt'"
            ),
            1,
            "{name}"
        ),
        "excluded" => assert_eq!(
            c("SELECT count(*) FROM NODES WHERE presence='excluded'"),
            1,
            "{name}"
        ),
        "changelist" => assert_eq!(
            c("SELECT count(*) FROM ACTUAL_NODE WHERE changelist='fixture-cl'"),
            2,
            "{name}"
        ),
        "switched" => assert!(
            c(
                "SELECT count(*) FROM NODES WHERE op_depth=0 AND repos_path LIKE 'branches/feature-x/%'"
            ) >= 1,
            "{name}"
        ),
        "mixed-rev" => assert!(
            c(
                "SELECT count(DISTINCT revision) FROM NODES WHERE op_depth=0 AND file_external IS NULL"
            ) >= 3,
            "{name}"
        ),
        "externals" => assert!(c("SELECT count(*) FROM EXTERNALS") >= 2, "{name}"),
        "added" | "copied" | "unicode-paths" | "replaced" | "replaced-dir" => assert!(
            c("SELECT count(*) FROM NODES WHERE op_depth > 0") >= 1,
            "{name}: no working node"
        ),
        "deleted" => assert!(
            c("SELECT count(*) FROM NODES WHERE op_depth > 0 AND presence='base-deleted'") >= 1,
            "{name}"
        ),
        "modified-props" | "merged-uncommitted" => assert!(
            c("SELECT count(*) FROM ACTUAL_NODE WHERE properties IS NOT NULL") >= 1,
            "{name}"
        ),
        "no-pristine" => {
            assert_eq!(fmt, 32, "{name} exists only in format 32");
            // Pristine rows are still recorded (checksums), but no file content is kept.
            assert!(c("SELECT count(*) FROM PRISTINE") >= 1, "{name}");
        }
        _ => {}
    }
    if !name.starts_with("conflict") && !name.starts_with("obstructed") {
        assert_eq!(c(conflicts), 0, "{name}: unexpected conflict");
    }
}

fn check_format(fmt_dir: &str, expected: i64, min_states: usize) {
    let m = manifest(fmt_dir);
    assert_eq!(m.format, expected);
    assert!(!m.svn_version.is_empty());
    assert!(
        m.states.len() >= min_states,
        "{fmt_dir}: only {} states",
        m.states.len()
    );
    let dir = fixtures().join(fmt_dir);
    for s in &m.states {
        assert!(
            !s.description.is_empty() && s.sha256.len() == 64,
            "{}",
            s.name
        );
        assert!(
            dir.join(&s.tarball).is_file(),
            "{}: tarball missing",
            s.name
        );
        let status = std::fs::read_to_string(dir.join(&s.status)).unwrap();
        assert!(status.starts_with("# svn status -v"), "{}", s.name);
    }
    if !tools_available() {
        return;
    }
    for s in &m.states {
        let tmp = tempfile::tempdir().unwrap();
        unpack(&dir.join(&s.tarball), tmp.path());
        let wc = tmp.path().join(&m.wc_dir_in_tarball);
        let db_path = wc.join(".svn/wc.db");
        let db = Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
        let fmt: i64 = db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fmt, expected, "{}: wc.db format", s.name);
        assert_state(expected, &s.name, &db);
        drop(db);
        assert_eq!(
            opensvn_compat::wcdb_dump::work_queue_rows(&wc).unwrap(),
            0,
            "{}: work queue left behind",
            s.name
        );
        let dump = opensvn_compat::wcdb_dump::dump_wc_db(&wc).unwrap();
        assert!(dump.contains("NODES"), "{}: dump has no NODES", s.name);
    }
}

#[test]
fn format_31_fixtures_cover_at_least_25_states() {
    check_format("f31", 31, 25);
}

#[test]
fn format_31_fixtures_from_svn_1_15_cover_at_least_42_states() {
    assert!(manifest("f31-svn1.15").svn_version.starts_with("1.15."));
    check_format("f31-svn1.15", 31, 42);
}

#[test]
fn format_32_fixtures_cover_at_least_43_states() {
    assert!(manifest("f32").svn_version.starts_with("1.15."));
    check_format("f32", 32, 43);
}

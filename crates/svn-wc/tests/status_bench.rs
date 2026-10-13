//! Timed status run on a synthetic 100k-file working copy (`#[ignore]`; run explicitly):
//!
//! ```text
//! cargo test --release -p svn-wc --test status_bench -- --ignored --nocapture
//! ```
//!
//! `OPENSVN_STATUS_BENCH_FILES` changes the file count (default 100 000, spread over
//! directories of 1 000 files). The working copy is built straight into wc.db (one shared
//! pristine, BASE rows without a recorded stat), so the first status compares every file with
//! its pristine and repairs the stat cache; the following runs take the size+mtime fast path,
//! which is the case the < 3 s target is about. In a release build the warm run is asserted
//! to stay under 3 s.
#![allow(clippy::expect_used, clippy::panic, clippy::print_stdout)]

use std::fs;
use std::time::{Duration, Instant};

use md5::Md5;
use sha1::{Digest, Sha1};
use svn_types::{CancelToken, Depth, RelPath};
use svn_wc::WcDb;
use svn_wc::db::NewWc;
use svn_wc::status::{DbPristineSource, ItemStatus, StatusOptions, status};

const PER_DIR: usize = 1_000;
const BODY: &[u8] = b"line one\nline two\nline three\n";

fn file_count() -> usize {
    std::env::var("OPENSVN_STATUS_BENCH_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000)
}

/// Builds `dNNN/fNNNN.txt` files plus their BASE rows; returns the open database.
fn build_wc(root: &std::path::Path, files: usize) -> WcDb {
    let db = WcDb::create(
        root,
        &NewWc {
            repos_root_url: "https://svn.example.invalid/repo".into(),
            repos_uuid: "00000000-0000-0000-0000-000000000000".into(),
            repos_relpath: "trunk".into(),
            revision: 0,
            depth: Depth::Infinity,
        },
    )
    .expect("create wc");

    let sha1_hex = hex(&Sha1::digest(BODY));
    let md5_hex = hex(&Md5::digest(BODY));
    let pristine_dir = root.join(".svn/pristine").join(&sha1_hex[..2]);
    fs::create_dir_all(&pristine_dir).expect("pristine dir");
    fs::write(pristine_dir.join(format!("{sha1_hex}.svn-base")), BODY).expect("pristine");

    let dirs = files.div_ceil(PER_DIR);
    for d in 0..dirs {
        let dir = root.join(format!("d{d:03}"));
        fs::create_dir(&dir).expect("mkdir");
        for f in 0..PER_DIR.min(files - d * PER_DIR) {
            fs::write(dir.join(format!("f{f:04}.txt")), BODY).expect("write");
        }
    }

    let wc_id = db.wc_id();
    db.with_write(|tx| {
        let e = svn_wc::db::sql_err;
        tx.execute(
            "INSERT INTO pristine (checksum, compression, size, refcount, md5_checksum) \
             VALUES (?1, NULL, ?2, ?3, ?4)",
            (
                format!("$sha1${sha1_hex}"),
                BODY.len() as i64,
                files as i64,
                format!("$md5 ${md5_hex}"),
            ),
        )
        .map_err(e)?;
        tx.execute(
            "UPDATE nodes SET presence = 'normal', revision = 1 WHERE wc_id = ?1 AND local_relpath = ''",
            [wc_id],
        )
        .map_err(e)?;
        let mut node = tx
            .prepare(
                "INSERT INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, repos_id, \
                 repos_path, revision, presence, depth, kind, checksum, changed_revision, \
                 changed_date, changed_author, translated_size, last_mod_time) \
                 VALUES (?1, ?2, 0, ?3, 1, ?4, 1, 'normal', ?5, ?6, ?7, 1, 0, 'bench', NULL, NULL)",
            )
            .map_err(e)?;
        for d in 0..dirs {
            let dir = format!("d{d:03}");
            node.execute((
                wc_id,
                &dir,
                "",
                format!("trunk/{dir}"),
                Some("infinity"),
                "dir",
                None::<String>,
            ))
            .map_err(e)?;
            for f in 0..PER_DIR.min(files - d * PER_DIR) {
                let path = format!("{dir}/f{f:04}.txt");
                node.execute((
                    wc_id,
                    &path,
                    &dir,
                    format!("trunk/{path}"),
                    None::<String>,
                    "file",
                    Some(format!("$sha1${sha1_hex}")),
                ))
                .map_err(e)?;
            }
        }
        Ok(())
    })
    .expect("insert nodes");
    db
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn timed(db: &WcDb, update_cache: bool) -> (Duration, Vec<svn_wc::status::StatusEntry>) {
    let opts = StatusOptions {
        update_cache,
        ..StatusOptions::default()
    };
    let start = Instant::now();
    let entries = status(
        db,
        &DbPristineSource(db),
        &RelPath::root(),
        &opts,
        &CancelToken::new(),
    )
    .expect("status");
    (start.elapsed(), entries)
}

#[test]
#[ignore = "benchmark: builds a 100k-file working copy; run with --release -- --ignored --nocapture"]
fn status_100k_files() {
    let files = file_count();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    let start = Instant::now();
    let db = build_wc(root, files);
    println!("build: {files} files in {:?}", start.elapsed());

    let (cold, entries) = timed(&db, true);
    assert!(
        entries.is_empty(),
        "clean wc reports {} entries",
        entries.len()
    );
    println!("cold status (content compare + cache repair): {cold:?}");
    let cached: i64 = db
        .with_read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM nodes WHERE kind = 'file' AND last_mod_time IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .map_err(svn_wc::db::sql_err)
        })
        .expect("count");
    assert_eq!(
        cached, files as i64,
        "the cold run repairs every stat cache entry"
    );

    let mut warm = Vec::new();
    for _ in 0..3 {
        let (t, entries) = timed(&db, false);
        assert!(entries.is_empty());
        warm.push(t);
    }
    println!("warm status x3: {warm:?}");

    fs::write(root.join("d000/f0000.txt"), b"changed\n").expect("modify");
    let (t, entries) = timed(&db, false);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].node_status, ItemStatus::Modified);
    println!("warm status with one modified file: {t:?}");

    let start = Instant::now();
    drop(db);
    tmp.close().expect("remove wc");
    println!(
        "teardown (close db, remove {files} files): {:?}",
        start.elapsed()
    );

    let best = warm.iter().min().copied().unwrap_or_default();
    if !cfg!(debug_assertions) {
        assert!(
            best < Duration::from_secs(3),
            "warm status of {files} files took {best:?} (target < 3 s)"
        );
    }
}

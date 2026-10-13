//! Byte-for-byte comparison with output recorded from Subversion 1.15.0.
//!
//! Inputs and expected outputs live in `tests/golden/`; regenerate them with
//! `python3 -I tests/gen/gen_golden.py` (needs a local `svn`).

// Test code: panicking on a broken fixture is the intended failure mode.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use svn_diff::{
    ConflictLabels, ConflictStyle, MergeOptions, UnifiedOptions, index_header, merge3, unified,
};

fn case_dirs(kind: &str) -> Vec<PathBuf> {
    // SVN_DIFF_GOLDEN_DIR lets a local soak run point at a larger generated corpus.
    let base = std::env::var_os("SVN_DIFF_GOLDEN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden"));
    let root = base.join(kind);
    let mut dirs: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read {}: {e}", root.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn read(dir: &Path, name: &str) -> Vec<u8> {
    fs::read(dir.join(name)).unwrap_or_else(|e| panic!("read {}/{name}: {e}", dir.display()))
}

fn show(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn unified_matches_svn_diff() {
    let dirs = case_dirs("unified");
    assert!(dirs.len() >= 40, "only {} unified cases", dirs.len());
    let mut failures = Vec::new();
    for dir in &dirs {
        let (orig, modi, expected) = (
            read(dir, "orig"),
            read(dir, "mod"),
            read(dir, "expected.diff"),
        );
        let text = show(&expected);
        let mut it = text.split_inclusive('\n');
        let index = format!("{}{}", it.next().unwrap_or(""), it.next().unwrap_or(""));
        let old_h = it
            .next()
            .unwrap_or("")
            .strip_prefix("--- ")
            .unwrap_or("")
            .trim_end_matches('\n')
            .to_owned();
        let new_h = it
            .next()
            .unwrap_or("")
            .strip_prefix("+++ ")
            .unwrap_or("")
            .trim_end_matches('\n')
            .to_owned();
        let opts = UnifiedOptions {
            original_header: old_h,
            modified_header: new_h,
            ..UnifiedOptions::default()
        };
        let got = unified(&orig, &modi, &opts);
        let body_start = index.len();
        if index != index_header("f") || got != expected[body_start..] {
            failures.push(format!(
                "{}\n--- expected\n{}\n--- got\n{}",
                dir.display(),
                &text[body_start..],
                show(&got)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} unified failures:\n{}",
        failures.len(),
        failures.join("\n=====\n")
    );
}

#[test]
fn merge3_matches_svn_update() {
    let dirs = case_dirs("merge3");
    assert!(dirs.len() >= 40, "only {} merge3 cases", dirs.len());
    let labels = ConflictLabels {
        mine: ".mine".into(),
        base: ".r1".into(),
        theirs: ".r2".into(),
    };
    let modes = [
        ("postpone", ConflictStyle::ModifiedOriginalLatest),
        ("mine-conflict", ConflictStyle::Modified),
        ("theirs-conflict", ConflictStyle::Latest),
    ];
    let mut failures = Vec::new();
    let mut conflicted = 0usize;
    for dir in &dirs {
        let (base, mine, theirs) = (read(dir, "base"), read(dir, "mine"), read(dir, "theirs"));
        for (mode, style) in modes {
            let expected = read(dir, &format!("expected.{mode}"));
            let opts = MergeOptions {
                style,
                labels: labels.clone(),
                ..MergeOptions::default()
            };
            let got = merge3(&base, &mine, &theirs, &opts);
            if mode == "postpone" && !got.conflicts.is_empty() {
                conflicted += 1;
            }
            if got.text != expected {
                failures.push(format!(
                    "{} [{mode}]\n--- expected\n{}\n--- got\n{}",
                    dir.display(),
                    show(&expected),
                    show(&got.text)
                ));
            }
        }
    }
    assert!(
        conflicted >= 20,
        "corpus has too few conflicting cases: {conflicted}"
    );
    assert!(
        failures.is_empty(),
        "{} merge3 failures:\n{}",
        failures.len(),
        failures.join("\n=====\n")
    );
}

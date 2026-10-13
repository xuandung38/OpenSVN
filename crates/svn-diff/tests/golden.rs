//! Byte-for-byte comparison with output recorded from Subversion 1.15.0.
//!
//! Inputs and expected outputs live in `tests/golden/`; regenerate them with
//! `python3 -I tests/gen/gen_golden.py` (needs a local `svn`).

// Test code: panicking on a broken fixture is the intended failure mode.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use svn_diff::{
    ConflictLabels, ConflictStyle, DiffOptions, IgnoreSpace, MergeOptions, UnifiedOptions,
    index_header, merge3, unified,
};
use svn_types::CancelToken;

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
        let diff = match fs::read(dir.join("opts")).ok().as_deref() {
            Some(b"w") => DiffOptions {
                ignore_space: IgnoreSpace::All,
                ..DiffOptions::default()
            },
            Some(b"b") => DiffOptions {
                ignore_space: IgnoreSpace::Change,
                ..DiffOptions::default()
            },
            Some(b"e") => DiffOptions {
                ignore_eol_style: true,
                ..DiffOptions::default()
            },
            None => DiffOptions::default(),
            Some(other) => panic!(
                "unrecognised opts {:?} in {}",
                String::from_utf8_lossy(other),
                dir.display()
            ),
        };
        let opts = UnifiedOptions {
            diff,
            // Fixtures were recorded with a Unix svn client.
            eol: "\n".into(),
            original_header: old_h,
            modified_header: new_h,
            ..UnifiedOptions::default()
        };
        let got = unified(&orig, &modi, &opts, &CancelToken::new()).expect("diff");
        let body_start = index.len();
        if index != index_header("f", "\n") || got != expected[body_start..] {
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
                fallback_eol: "\n".into(),
                ..MergeOptions::default()
            };
            let got = merge3(&base, &mine, &theirs, &opts, &CancelToken::new()).expect("merge");
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

/// Removes the `|||||||` base sections from svn's marker output, which is exactly what the
/// `ModifiedLatest` style prints.  Only valid when every marker starts its own line.
fn strip_base_sections(text: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut in_base = false;
    for line in svn_diff::split_lines(text) {
        if line.starts_with(b"||||||| ") {
            in_base = true;
        } else if line.starts_with(b"=======") {
            in_base = false;
        }
        if !in_base {
            out.extend_from_slice(line);
        }
    }
    Some(out)
}

#[test]
fn modified_latest_matches_svn_markers_without_base_section() {
    let labels = ConflictLabels {
        mine: ".mine".into(),
        base: ".r1".into(),
        theirs: ".r2".into(),
    };
    let mut checked = 0usize;
    for dir in case_dirs("merge3") {
        let (base, mine, theirs) = (read(&dir, "base"), read(&dir, "mine"), read(&dir, "theirs"));
        let expected = read(&dir, "expected.postpone");
        // Skip fixtures whose markers are glued to an unterminated line.
        let glued = svn_diff::split_lines(&expected).iter().any(|l| {
            [&b"|||||||"[..], b"=======", b">>>>>>>"]
                .iter()
                .any(|m| !l.starts_with(m) && l.windows(7).any(|w| w == *m))
        });
        if glued || !expected.windows(8).any(|w| w == b"||||||| ") {
            continue;
        }
        let opts = MergeOptions {
            style: ConflictStyle::ModifiedLatest,
            labels: labels.clone(),
            fallback_eol: "\n".into(),
            ..MergeOptions::default()
        };
        let got = merge3(&base, &mine, &theirs, &opts, &CancelToken::new()).expect("merge");
        assert_eq!(
            got.text,
            strip_base_sections(&expected).unwrap_or_default(),
            "{}",
            dir.display()
        );
        checked += 1;
    }
    assert!(checked >= 20, "only {checked} conflicting fixtures checked");
}

#[test]
fn resolved_modified_latest_narrows_conflicts_around_shared_lines() {
    let dir = case_dirs("merge3")
        .into_iter()
        .find(|d| d.ends_with("partial-overlap-common-tail"))
        .expect("fixture");
    let (base, mine, theirs) = (read(&dir, "base"), read(&dir, "mine"), read(&dir, "theirs"));
    let opts = MergeOptions {
        style: ConflictStyle::ResolvedModifiedLatest,
        labels: ConflictLabels {
            mine: ".mine".into(),
            base: ".r1".into(),
            theirs: ".r2".into(),
        },
        fallback_eol: "\n".into(),
        ..MergeOptions::default()
    };
    let got = merge3(&base, &mine, &theirs, &opts, &CancelToken::new()).expect("merge");
    let text = show(&got.text);
    // The shared trailing line is no longer inside a conflict, and no base section is shown.
    assert!(
        text.contains("<<<<<<< .mine\nm5\nline 6x\n=======\nt5\nline 6y\n>>>>>>> .r2\nshared\n"),
        "{text}"
    );
    assert!(!text.contains("|||||||"));
}

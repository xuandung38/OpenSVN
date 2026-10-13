//! Compares translation output with working copies produced by a real svn client
//! (fixtures in `tests/compat/scenarios/wc/translate`, regenerate with `generate.py`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use svn_wc::translate::special::{self, SpecialContent};
use svn_wc::translate::{
    EolStyle, KeywordContext, TranslateError, TranslateInfo, to_repo_normal, to_working_with_native,
};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/compat/scenarios/wc/translate")
}

/// Directories holding a `meta.txt`.
fn fixture_dirs(dir: &Path, out: &mut Vec<PathBuf>) {
    if dir.join("meta.txt").exists() {
        out.push(dir.to_path_buf());
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut subs: Vec<_> = rd
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    subs.sort();
    for s in subs {
        fixture_dirs(&s, out);
    }
}

fn read_meta(dir: &Path) -> BTreeMap<String, String> {
    fs::read_to_string(dir.join("meta.txt"))
        .expect("meta.txt")
        .lines()
        .filter_map(|l| {
            l.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect()
}

fn info_from_meta(meta: &BTreeMap<String, String>) -> TranslateInfo {
    TranslateInfo::from_props(|name| meta.get(&format!("prop.{name}")).map(String::as_bytes))
}

#[test]
fn matches_real_svn_output() {
    let mut dirs = Vec::new();
    fixture_dirs(&fixture_root(), &mut dirs);
    assert!(dirs.len() > 30, "fixtures missing: {}", dirs.len());
    let mut checked = 0;
    for dir in dirs {
        let meta = read_meta(&dir);
        let info = info_from_meta(&meta);
        let label = dir
            .strip_prefix(fixture_root())
            .unwrap()
            .display()
            .to_string();

        if dir.join("expect_error.txt").exists() {
            // svn refused to commit: our normalisation must refuse too where svn did so at commit.
            let working = fs::read(dir.join("working.bin")).unwrap();
            match info.eol_style {
                EolStyle::Native => assert!(
                    matches!(
                        to_repo_normal(&info, &working, false),
                        Err(TranslateError::InconsistentEol)
                    ),
                    "{label}: expected inconsistent eol"
                ),
                EolStyle::Unknown => assert!(
                    matches!(
                        to_repo_normal(&info, &working, false),
                        Err(TranslateError::UnknownEol)
                    ),
                    "{label}: expected unknown eol"
                ),
                _ => {}
            }
            checked += 1;
            continue;
        }

        let pristine = fs::read(dir.join("pristine.bin")).unwrap();
        if let Ok(target) = fs::read_to_string(dir.join("working.link")) {
            assert_eq!(
                special::parse(&pristine),
                SpecialContent::Link(target.clone().into_bytes()),
                "{label}"
            );
            assert_eq!(
                special::symlink_to_repo_normal(target.as_bytes()),
                pristine,
                "{label}"
            );
            checked += 1;
            continue;
        }

        let working = fs::read(dir.join("working.bin")).unwrap();
        let ctx = KeywordContext {
            rev: Some(meta["rev"].parse().unwrap()),
            url: Some(meta["url"].clone()),
            repos_root_url: Some(meta["root"].clone()),
            date_micros: Some(meta["date_micros"].parse().unwrap()),
            author: Some(meta["author"].clone()),
            utc_offset_secs: meta["utc_offset_secs"].parse().unwrap(),
        };
        let got = to_working_with_native(&info, &pristine, &ctx, b"\n").unwrap();
        assert_eq!(
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(&working),
            "{label}: to_working"
        );
        assert_eq!(got, working, "{label}: to_working bytes");

        // Some inputs are not stable under expand+contract in svn itself (the fixed-length
        // keyword straddling case); only the forward direction is compared for those.
        if label.contains("lossy") {
            checked += 1;
            continue;
        }
        let back = to_repo_normal(&info, &working, false).unwrap();
        assert_eq!(back, pristine, "{label}: to_repo_normal");
        checked += 1;
    }
    assert!(checked > 30);
}

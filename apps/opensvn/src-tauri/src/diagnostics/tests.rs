#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use opensvn_ipc::Settings;

use super::zip::tests_support::read_zip;
use super::*;
use crate::settings::defaults;

fn inputs(logs: Option<std::path::PathBuf>) -> BundleInputs {
    BundleInputs {
        app_version: "0.0.0".into(),
        platform: "test x86_64".into(),
        settings: defaults(None),
        working_copies: vec![("Secret project".into(), "/Users/alice/src/proj".into())],
        home_dir: Some("/Users/alice".into()),
        logs_dir: logs,
    }
}

fn some_settings() -> Settings {
    let mut s = defaults(None);
    s.general_checkout_root = "/Users/alice/svn".into();
    s
}

#[test]
fn preview_lists_the_files_with_their_full_content() {
    let svc = DiagnosticsService::default();
    let p = svc.preview(&inputs(None), DiagnosticsOptions::default());
    let names: Vec<_> = p.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        ["app-info.txt", "settings.json", "working-copies.txt"]
    );
    assert!(p.entries[2].content.contains("/Users/alice/src/proj"));
    assert_eq!(p.entries[0].size as usize, p.entries[0].content.len());
}

#[test]
fn hide_options_remove_paths_names_and_commit_messages() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("opensvn.2026-10-13.log"),
        "opened /Users/alice/src/proj/a.rs\ncommit message=\"fix leak\" rev=2\nAuthorization: Basic QUJD\nhttps://bob:pw123456@svn.test.local/x\n",
    )
    .unwrap();
    let mut i = inputs(Some(dir.path().to_path_buf()));
    i.settings = some_settings();
    let svc = DiagnosticsService::default();
    let p = svc.preview(
        &i,
        DiagnosticsOptions {
            hide_paths: true,
            hide_commit_messages: true,
        },
    );
    let all: String = p
        .entries
        .iter()
        .map(|e| e.content.clone())
        .collect::<Vec<_>>()
        .join("\n");
    for hidden in ["alice", "Secret project", "fix leak", "QUJD", "pw123456"] {
        assert!(!all.contains(hidden), "{hidden} leaked:\n{all}");
    }
    assert!(all.contains("<wc-1>"));
    assert!(
        p.entries
            .iter()
            .any(|e| e.name == "logs/opensvn.2026-10-13.log")
    );
}

#[test]
fn secrets_are_removed_even_when_nothing_else_is_hidden() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.log"), "Cookie: sid=topsecret\n").unwrap();
    let svc = DiagnosticsService::default();
    let p = svc.preview(
        &inputs(Some(dir.path().to_path_buf())),
        DiagnosticsOptions::default(),
    );
    assert!(p.entries.iter().all(|e| !e.content.contains("topsecret")));
}

#[test]
fn export_writes_exactly_the_previewed_files_once() {
    let svc = DiagnosticsService::default();
    let p = svc.preview(&inputs(None), DiagnosticsOptions::default());
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("diag.zip");
    assert!(svc.export("wrong-token", &dest).is_err());
    assert!(!dest.exists());
    svc.export(&p.token, &dest).unwrap();
    let back = read_zip(&std::fs::read(&dest).unwrap());
    assert_eq!(back.len(), p.entries.len());
    for (entry, (name, bytes)) in p.entries.iter().zip(back) {
        assert_eq!(entry.name, name);
        assert_eq!(entry.content.as_bytes(), bytes.as_slice());
    }
    // the preview is spent
    assert!(svc.export(&p.token, &dest).is_err());
}

#[test]
fn a_new_preview_replaces_the_old_one() {
    let svc = DiagnosticsService::default();
    let first = svc.preview(&inputs(None), DiagnosticsOptions::default());
    let second = svc.preview(
        &inputs(None),
        DiagnosticsOptions {
            hide_paths: true,
            hide_commit_messages: false,
        },
    );
    let dir = tempfile::tempdir().unwrap();
    assert!(svc.export(&first.token, &dir.path().join("a.zip")).is_err());
    svc.export(&second.token, &dir.path().join("b.zip"))
        .unwrap();
}

#[test]
fn old_logs_and_huge_logs_are_trimmed() {
    let dir = tempfile::tempdir().unwrap();
    let big = "line of text\n".repeat(30_000); // ~390 KB
    std::fs::write(dir.path().join("big.log"), &big).unwrap();
    std::fs::write(dir.path().join("notes.txt"), "not a log").unwrap();
    std::fs::write(
        dir.path().join("opensvn.log.1"),
        "rotated Authorization: Bearer ghp_rotatedsecret\n",
    )
    .unwrap();
    let svc = DiagnosticsService::default();
    let p = svc.preview(
        &inputs(Some(dir.path().to_path_buf())),
        DiagnosticsOptions::default(),
    );
    let log = p.entries.iter().find(|e| e.name == "logs/big.log").unwrap();
    assert!(log.content.len() <= 200 * 1024);
    assert!(
        log.content.starts_with("line of text"),
        "the tail starts at a line boundary"
    );
    assert!(p.entries.iter().all(|e| e.name != "logs/notes.txt"));
    let rotated = p
        .entries
        .iter()
        .find(|e| e.name == "logs/opensvn.log.1")
        .expect("the rotated log is included");
    assert!(rotated.content.starts_with("rotated"));
    assert!(
        !rotated.content.contains("ghp_rotatedsecret"),
        "the rotated log is redacted too"
    );
}

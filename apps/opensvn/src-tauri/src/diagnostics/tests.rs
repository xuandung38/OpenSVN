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

mod secrets_never_reach_the_zip {
    use proptest::prelude::*;

    use super::*;
    use opensvn_ipc::{ExtDiffTool, ExternalDiffTool, ProxySetting};

    /// Log lines in the shapes the app and its libraries write, each carrying `{s}`.
    const LOG_LINES: &[&str] = &[
        "GET https://alice:{s}@svn.test/repo/trunk HTTP/1.1",
        "Authorization: Basic {s}",
        "authorization: Negotiate {s}",
        "Proxy-Authorization: Basic {s}",
        "Cookie: sid={s}",
        "Cookie: theme=dark; sid={s}; lang=vi",
        "Set-Cookie: sid={s}; Path=/svn; Secure; HttpOnly",
        "{\"password\":\"{s}\",\"user\":\"alice\"}",
        "password = {s}",
        "PASSWD: {s}",
        "lock-token: <opaquelocktoken:{s}>",
        "If: (<opaquelocktoken:{s}>)",
        "token={s}&rev=3",
        "x-api-key: {s}",
        "X-Auth-Token: {s}",
        "level=DEBUG proxy=\"http://bob:{s}@proxy.test:3128\" connecting",
        "2026-10-15T01:02:03Z WARN request failed url=https://u:{s}@svn.test/r secret=\"{s}\"",
    ];

    /// Characters a secret may hold in every line shape above. No `/`: inside a URL's userinfo
    /// it is percent-encoded (`%2F`), as the URL parser writes it.
    fn secret() -> impl Strategy<Value = String> {
        "[A-Za-z0-9+=_.~%-]{12,32}"
    }

    fn settings_with(s: &str, place: u8) -> opensvn_ipc::Settings {
        let mut settings = defaults(None);
        let tool = |args: Vec<String>| ExternalDiffTool {
            path: "/usr/bin/meld".into(),
            args,
        };
        match place % 5 {
            0 => {
                settings.network_proxy = ProxySetting::Custom {
                    url: format!("http://proxy-user:{s}@proxy.test:3128"),
                };
            }
            1 => {
                settings.integrations_web_revision_url =
                    format!("https://web.test/r/{{rev}}?token={s}");
            }
            2 => {
                settings.integrations_web_file_url =
                    format!("https://viewer:{s}@web.test/f/{{path}}");
            }
            3 => {
                settings.diff_external_tool =
                    Some(tool(vec!["--password".into(), format!("password={s}")]));
            }
            _ => {
                settings.diff_tools_by_extension = Some(vec![ExtDiffTool {
                    ext: "docx".into(),
                    path: "/opt/diff".into(),
                    args: vec![format!("--token={s}"), "{left}".into(), "{right}".into()],
                }]);
            }
        }
        settings
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Preview then export, as the dialog does, with random secrets in random log lines and in
        /// one setting: neither the preview nor any byte of the saved zip holds a secret.
        #[test]
        fn preview_then_export(
            secrets in prop::collection::vec(secret(), 1..4),
            lines in prop::collection::vec((0..LOG_LINES.len(), 0usize..4), 1..12),
            place in 0u8..5,
            hide_paths in any::<bool>(),
            hide_commit_messages in any::<bool>(),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let mut log = String::from("2026-10-15T01:00:00Z INFO opensvn started\n");
            for (line, which) in &lines {
                let s = &secrets[which % secrets.len()];
                log.push_str(&LOG_LINES[*line].replace("{s}", s));
                log.push('\n');
            }
            std::fs::write(dir.path().join("opensvn.2026-10-15.log"), &log).unwrap();
            std::fs::write(dir.path().join("opensvn.log.1"), &log).unwrap();
            let mut i = inputs(Some(dir.path().to_path_buf()));
            i.settings = settings_with(&secrets[0], place);

            let svc = DiagnosticsService::default();
            let p = svc.preview(&i, DiagnosticsOptions { hide_paths, hide_commit_messages });
            prop_assert!(p.entries.iter().any(|e| e.name == "logs/opensvn.log.1"));
            let dest = dir.path().join("diag.zip");
            svc.export(&p.token, &dest).unwrap();
            let zip = std::fs::read(&dest).unwrap();
            for s in &secrets {
                for e in &p.entries {
                    prop_assert!(!e.content.contains(s.as_str()), "{} leaked in {}:\n{}", s, e.name, e.content);
                }
                prop_assert!(!contains(&zip, s.as_bytes()), "{} leaked in the zip", s);
            }
        }
    }
}

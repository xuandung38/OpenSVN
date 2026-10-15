#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Mutex as StdMutex;

use super::*;
use crate::db::AppDb;

struct Scripted {
    pick: Option<PathBuf>,
    accept: bool,
    confirmed: StdMutex<Vec<String>>,
}

#[async_trait]
impl ProgramPicker for Scripted {
    async fn pick_program(&self, _kind: ProgramKind) -> Option<PathBuf> {
        self.pick.clone()
    }
    async fn confirm(&self, _kind: ProgramKind, command_line: &str) -> bool {
        self.confirmed.lock().unwrap().push(command_line.to_owned());
        self.accept
    }
}

fn service(pick: Option<PathBuf>, accept: bool) -> (ProgramService, Arc<Scripted>) {
    let settings = Arc::new(SettingsService::load(AppDb::open_in_memory().unwrap(), None).unwrap());
    let picker = Arc::new(Scripted {
        pick,
        accept,
        confirmed: StdMutex::new(Vec::new()),
    });
    (ProgramService::new(settings, picker.clone()), picker)
}

/// A real executable file that exists on every host running the tests.
fn an_executable() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let name = if cfg!(windows) { "tool.exe" } else { "tool" };
    let path = dir.path().join(name);
    std::fs::write(&path, b"#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (dir, path)
}

#[tokio::test]
async fn confirmed_pick_is_stored_with_the_canonical_path() {
    let (_dir, exe) = an_executable();
    let (svc, picker) = service(Some(exe.clone()), true);
    let s = svc
        .choose(ProgramKind::Editor, vec!["--wait".into()])
        .await
        .unwrap();
    let stored = s.tools_editor.unwrap();
    assert_eq!(
        stored.path,
        std::fs::canonicalize(&exe).unwrap().to_string_lossy()
    );
    assert_eq!(stored.args, vec!["--wait"]);
    // the user saw the full command line before it was saved
    let shown = picker.confirmed.lock().unwrap().clone();
    assert_eq!(shown, vec![format!("{}\n  \"--wait\"", stored.path)]);
}

#[tokio::test]
async fn cancel_and_decline_change_nothing() {
    let (_dir, exe) = an_executable();
    let (svc, _) = service(None, true);
    assert_eq!(
        svc.choose(ProgramKind::Terminal, vec![])
            .await
            .unwrap()
            .tools_terminal,
        None
    );
    let (svc, picker) = service(Some(exe), false);
    assert_eq!(
        svc.choose(ProgramKind::Terminal, vec![])
            .await
            .unwrap()
            .tools_terminal,
        None
    );
    assert_eq!(
        picker.confirmed.lock().unwrap().len(),
        1,
        "asked, then declined"
    );
}

#[tokio::test]
async fn rejects_missing_directories_and_non_executables() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, picker) = service(Some(dir.path().join("missing")), true);
    assert!(svc.choose(ProgramKind::DiffTool, vec![]).await.is_err());
    let (svc, _) = service(Some(dir.path().to_path_buf()), true);
    assert!(svc.choose(ProgramKind::DiffTool, vec![]).await.is_err());
    #[cfg(unix)]
    {
        let plain = dir.path().join("data.txt");
        std::fs::write(&plain, b"x").unwrap();
        let (svc, picker2) = service(Some(plain), true);
        assert!(svc.choose(ProgramKind::DiffTool, vec![]).await.is_err());
        assert!(
            picker2.confirmed.lock().unwrap().is_empty(),
            "no confirmation for an invalid file"
        );
    }
    assert!(picker.confirmed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn rejects_oversized_or_nul_arguments_before_any_dialog() {
    let (_dir, exe) = an_executable();
    let (svc, picker) = service(Some(exe), true);
    assert!(
        svc.choose(ProgramKind::Editor, vec!["a\0b".into()])
            .await
            .is_err()
    );
    assert!(
        svc.choose(ProgramKind::Editor, vec!["x".repeat(2000)])
            .await
            .is_err()
    );
    assert!(
        svc.choose(ProgramKind::Editor, vec!["x".into(); 33])
            .await
            .is_err()
    );
    assert!(picker.confirmed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn clear_removes_the_setting() {
    let (_dir, exe) = an_executable();
    let (svc, _) = service(Some(exe), true);
    svc.choose(ProgramKind::MergeTool, vec![]).await.unwrap();
    assert_eq!(
        svc.clear(ProgramKind::MergeTool).unwrap().diff_merge_tool,
        None
    );
}

#[tokio::test]
async fn second_request_while_a_dialog_is_open_is_refused() {
    struct Slow;
    #[async_trait]
    impl ProgramPicker for Slow {
        async fn pick_program(&self, _: ProgramKind) -> Option<PathBuf> {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            None
        }
        async fn confirm(&self, _: ProgramKind, _: &str) -> bool {
            false
        }
    }
    let settings = Arc::new(SettingsService::load(AppDb::open_in_memory().unwrap(), None).unwrap());
    let svc = Arc::new(ProgramService::new(settings, Arc::new(Slow)));
    let first = {
        let svc = svc.clone();
        tokio::spawn(async move { svc.choose(ProgramKind::Editor, vec![]).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(svc.choose(ProgramKind::Editor, vec![]).await.is_err());
    first.await.unwrap().unwrap();
}

#[tokio::test]
async fn deceptive_characters_are_refused_in_arguments() {
    let (_dir, exe) = an_executable();
    let (svc, picker) = service(Some(exe), true);
    for bad in [
        "a\nb",
        "a\rb",
        "a\u{202E}b",
        "a\u{200B}b",
        "\u{7f}",
        "\u{85}",
        "\u{2066}x",
        "a\u{2028}b",
        "a\u{2029}b",
        "a\u{061C}b",
        "a\u{180E}b",
        "a\u{00AD}b",
    ] {
        assert!(
            svc.choose(ProgramKind::Editor, vec![bad.into()])
                .await
                .is_err(),
            "{bad:?}"
        );
    }
    assert!(
        svc.choose(ProgramKind::Editor, vec!["x".repeat(1000); 3])
            .await
            .is_err(),
        "total length"
    );
    assert!(picker.confirmed.lock().unwrap().is_empty());
}

#[test]
fn dialog_text_quotes_every_argument_on_its_own_line() {
    let text = command_line_for_dialog(
        "/bin/ed",
        &["a b".into(), "he said \"hi\"".into(), "back\\slash".into()],
    );
    assert_eq!(
        text,
        "/bin/ed\n  \"a b\"\n  \"he said \\\"hi\\\"\"\n  \"back\\\\slash\""
    );
}

#[test]
fn verbatim_prefix_is_removed_only_for_drive_paths() {
    assert_eq!(strip_verbatim(r"\\?\C:\Tools\ed.exe"), r"C:\Tools\ed.exe");
    assert_eq!(
        strip_verbatim(r"\\?\UNC\srv\share\ed.exe"),
        r"\\?\UNC\srv\share\ed.exe"
    );
    assert_eq!(strip_verbatim("/usr/bin/ed"), "/usr/bin/ed");
}

#[tokio::test]
async fn a_diff_tool_per_extension_is_confirmed_stored_replaced_and_cleared() {
    let (_dir, exe) = an_executable();
    let (svc, picker) = service(Some(exe.clone()), true);
    let s = svc
        .choose_for_extension(".DOCX", vec!["{left}".into(), "{right}".into()])
        .await
        .unwrap();
    let tools = s.diff_tools_by_extension.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].ext, "docx");
    assert!(picker.confirmed.lock().unwrap()[0].starts_with("Diff tool for .docx files:\n"));

    // The tool for the extension wins over the global one; other files use the global one.
    let (program, args, by_ext) = svc.external_diff_tool_for("a/b/Report.docx").unwrap();
    assert!(by_ext && args == ["{left}", "{right}"]);
    assert_eq!(program, std::fs::canonicalize(&exe).unwrap());
    let err = svc.external_diff_tool_for("a.txt").unwrap_err();
    assert_eq!(err.actions, Some(vec![ErrorAction::OpenSettings]));

    let s = svc
        .choose_for_extension("docx", vec!["--compare".into()])
        .await
        .unwrap();
    let tools = s.diff_tools_by_extension.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].args, ["--compare"]);
    svc.choose_for_extension("xlsx", vec![]).await.unwrap();

    for bad in ["", ".", "do cx", "x.y", "dóc", "abcdefghijklmnopq"] {
        assert!(
            svc.choose_for_extension(bad, vec![]).await.is_err(),
            "{bad}"
        );
    }
    let s = svc.clear_for_extension(".docx").unwrap();
    let tools = s.diff_tools_by_extension.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].ext, "xlsx");
    assert!(
        svc.clear_for_extension("xlsx")
            .unwrap()
            .diff_tools_by_extension
            .is_none()
    );
}

#[tokio::test]
async fn a_declined_extension_tool_is_not_stored() {
    let (_dir, exe) = an_executable();
    let (svc, _) = service(Some(exe), false);
    let s = svc.choose_for_extension("docx", vec![]).await.unwrap();
    assert!(s.diff_tools_by_extension.is_none());
}

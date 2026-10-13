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
    assert_eq!(shown, vec![format!("{} --wait", stored.path)]);
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

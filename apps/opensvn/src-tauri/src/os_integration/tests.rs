#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Mutex;

use super::*;

#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<String>>,
}

impl OsOpener for Recorder {
    fn reveal(&self, path: &Path) -> Result<(), AppError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("reveal {}", path.display()));
        Ok(())
    }
    fn open_default(&self, path: &Path) -> Result<(), AppError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("open {}", path.display()));
        Ok(())
    }
    fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<(), AppError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("run {program} {args:?} in {}", cwd.display()));
        Ok(())
    }
}

fn wc() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("src")).unwrap();
    std::fs::write(d.path().join("src/main.rs"), b"fn main() {}").unwrap();
    std::fs::write(d.path().join("setup.exe"), b"MZ").unwrap();
    std::fs::write(d.path().join("run.sh"), b"#!/bin/sh\n").unwrap();
    std::fs::write(d.path().join("README.MD"), b"hi").unwrap();
    d
}

fn tool(path: &str, args: &[&str]) -> ExternalDiffTool {
    ExternalDiffTool {
        path: path.into(),
        args: args.iter().map(|s| (*s).to_owned()).collect(),
    }
}

#[test]
fn paths_outside_the_working_copy_or_missing_are_refused() {
    let d = wc();
    let r = Recorder::default();
    for bad in ["../outside", "/etc/passwd", "a/../../b", "src/../../x"] {
        let err = reveal_in_os(&r, d.path(), bad).unwrap_err();
        assert_eq!(
            err.code,
            ErrorCode::Internal,
            "{bad}: unsafe path maps to internal"
        );
    }
    let err = reveal_in_os(&r, d.path(), "src/missing.rs").unwrap_err();
    assert_eq!(err.code, ErrorCode::PathNotFound);
    assert!(r.calls.lock().unwrap().is_empty());
}

#[test]
fn reveal_uses_the_resolved_path() {
    let d = wc();
    let r = Recorder::default();
    reveal_in_os(&r, d.path(), "src/main.rs").unwrap();
    reveal_in_os(&r, d.path(), "").unwrap();
    let calls = r.calls.lock().unwrap().clone();
    assert!(calls[0].ends_with("main.rs") && calls[0].starts_with("reveal "));
    assert_eq!(calls.len(), 2);
}

#[test]
fn default_app_never_opens_files_that_run_code() {
    let d = wc();
    let r = Recorder::default();
    for runnable in ["setup.exe", "run.sh"] {
        let err = open_external(&r, d.path(), runnable, OpenWith::Default, None, None).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{runnable}");
    }
    open_external(&r, d.path(), "README.MD", OpenWith::Default, None, None).unwrap();
    open_external(&r, d.path(), "src", OpenWith::Default, None, None).unwrap();
    assert_eq!(
        r.calls.lock().unwrap().len(),
        2,
        "only the document and the folder were opened"
    );
}

#[cfg(unix)]
#[test]
fn an_execute_bit_also_blocks_the_default_app() {
    use std::os::unix::fs::PermissionsExt;
    let d = wc();
    let f = d.path().join("tool");
    std::fs::write(&f, b"binary").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(is_runnable(&f));
    assert!(!is_runnable(&d.path().join("README.MD")));
}

#[test]
fn editor_gets_its_configured_args_then_the_file_and_terminal_gets_the_folder() {
    let d = wc();
    let r = Recorder::default();
    let editor = tool("/usr/bin/code", &["--wait"]);
    let terminal = tool("/usr/bin/term", &["--new-window"]);
    open_external(
        &r,
        d.path(),
        "src/main.rs",
        OpenWith::Editor,
        Some(&editor),
        None,
    )
    .unwrap();
    open_external(
        &r,
        d.path(),
        "src/main.rs",
        OpenWith::Terminal,
        None,
        Some(&terminal),
    )
    .unwrap();
    open_external(
        &r,
        d.path(),
        "src",
        OpenWith::Terminal,
        None,
        Some(&terminal),
    )
    .unwrap();
    let calls = r.calls.lock().unwrap().clone();
    assert!(
        calls[0].starts_with("run /usr/bin/code [\"--wait\", \""),
        "{}",
        calls[0]
    );
    assert!(calls[0].contains("main.rs\"]"));
    assert!(
        calls[1].contains("[\"--new-window\"]") && calls[1].ends_with("src"),
        "{}",
        calls[1]
    );
    assert_eq!(
        calls[1], calls[2],
        "a file and its folder open the terminal in the same place"
    );
}

#[test]
fn unconfigured_editor_and_terminal_point_to_settings() {
    let d = wc();
    let r = Recorder::default();
    for with in [OpenWith::Editor, OpenWith::Terminal] {
        let err = open_external(&r, d.path(), "src/main.rs", with, None, None).unwrap_err();
        assert_eq!(err.actions, Some(vec![ErrorAction::OpenSettings]));
    }
}

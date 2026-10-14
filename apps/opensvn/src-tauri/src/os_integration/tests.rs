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

fn refused_with_reveal(err: &AppError, what: &str) {
    assert_eq!(err.code, ErrorCode::InvalidArgument, "{what}");
    assert_eq!(
        err.actions,
        Some(vec![ErrorAction::RevealInFileManager]),
        "{what}: the UI offers reveal instead"
    );
}

#[test]
fn default_app_never_opens_files_that_run_code() {
    let d = wc();
    let r = Recorder::default();
    for runnable in ["setup.exe", "run.sh", "src/main.rs"] {
        let err = open_external(&r, d.path(), runnable, OpenWith::Default, None, None).unwrap_err();
        refused_with_reveal(&err, runnable);
    }
    open_external(&r, d.path(), "README.MD", OpenWith::Default, None, None).unwrap();
    open_external(&r, d.path(), "src", OpenWith::Default, None, None).unwrap();
    assert_eq!(
        r.calls.lock().unwrap().len(),
        2,
        "only the document and the folder were opened"
    );
}

#[test]
fn default_app_uses_an_allow_list_not_a_block_list() {
    let d = wc();
    let r = Recorder::default();
    let names = [
        "notes.TXT",
        "a.Pdf",
        "pic.png",
        "sheet.xlsx",
        "x.terminal",
        "x.webloc",
        "x.inetloc",
        "x.fileloc",
        "x.chm",
        "x.msc",
        "x.scf",
        "x.jnlp",
        "x.appref-ms",
        "x.application",
        "x.settingcontent-ms",
        "x.inf",
        "x.sct",
        "x.wsc",
        "x.xll",
        "x.library-ms",
        "x.search-ms",
        "x.diagcab",
        "x.iso",
        "x.img",
        "x.vhd",
        "x.vhdx",
        "x.gadget",
        "x.ps1xml",
        "x.psc1",
        "x.msix",
        "x.appx",
        "x.appinstaller",
        "x.zip",
        "x.html",
        "x.svg",
        "x.docm",
        "x.xlsm",
        // Legacy binary office formats can carry macros under an innocent extension; OpenDocument
        // formats can carry Basic macros; RTF can embed OLE objects.
        "x.doc",
        "x.xls",
        "x.ppt",
        "x.odt",
        "x.ods",
        "x.odp",
        "x.rtf",
        "Makefile",
        ".txt",
        "x.unknown",
    ];
    for n in names {
        std::fs::write(d.path().join(n), b"x").unwrap();
    }
    for n in names {
        let res = open_external(&r, d.path(), n, OpenWith::Default, None, None);
        let allowed = ["notes.TXT", "a.Pdf", "pic.png", "sheet.xlsx"].contains(&n);
        match res {
            Ok(()) => assert!(allowed, "{n} must not open"),
            Err(e) => {
                assert!(!allowed, "{n} should open");
                refused_with_reveal(&e, n);
            }
        }
    }
    assert_eq!(r.calls.lock().unwrap().len(), 4);
}

#[test]
fn trailing_dots_and_spaces_are_refused_before_any_extension_check() {
    let d = wc();
    let r = Recorder::default();
    for bad in [
        "evil.exe.",
        "evil.exe ",
        "dir./a.txt",
        "notes.txt.",
        "CON.txt",
        "a:b.txt",
    ] {
        let err = open_external(&r, d.path(), bad, OpenWith::Default, None, None).unwrap_err();
        assert_ne!(err.code, ErrorCode::PathNotFound, "{bad}: refused by name");
        let err = reveal_in_os(&r, d.path(), bad).unwrap_err();
        assert_ne!(err.code, ErrorCode::PathNotFound, "{bad}: refused by name");
    }
    assert!(r.calls.lock().unwrap().is_empty());
    // Even on its own, the allow-list check trims what Windows trims.
    assert!(!default_app_allowed(Path::new("evil.exe.")));
    assert!(!default_app_allowed(Path::new("evil.exe ")));
}

#[test]
fn folders_that_may_be_bundles_are_refused() {
    let d = wc();
    let r = Recorder::default();
    for b in ["Evil.app", "Thing.prefPane", "v1.2"] {
        std::fs::create_dir_all(d.path().join(b)).unwrap();
        let err = open_external(&r, d.path(), b, OpenWith::Default, None, None).unwrap_err();
        refused_with_reveal(&err, b);
        reveal_in_os(&r, d.path(), b).unwrap();
    }
    assert!(
        r.calls
            .lock()
            .unwrap()
            .iter()
            .all(|c| c.starts_with("reveal "))
    );
}

#[cfg(unix)]
#[test]
fn symlinks_are_never_opened_with_the_default_app() {
    use std::os::unix::fs::symlink;
    let d = wc();
    let r = Recorder::default();
    std::fs::create_dir_all(d.path().join("Evil.app")).unwrap();
    std::fs::write(d.path().join("x.terminal"), b"x").unwrap();
    symlink("Evil.app", d.path().join("README.txt")).unwrap();
    symlink("x.terminal", d.path().join("notes.txt")).unwrap();
    std::fs::create_dir_all(d.path().join("real")).unwrap();
    std::fs::write(d.path().join("real/doc.txt"), b"x").unwrap();
    symlink("real", d.path().join("linked")).unwrap();
    for n in ["README.txt", "notes.txt", "linked"] {
        let err = open_external(&r, d.path(), n, OpenWith::Default, None, None).unwrap_err();
        refused_with_reveal(&err, n);
    }
    assert!(r.calls.lock().unwrap().is_empty());
    open_external(&r, d.path(), "real/doc.txt", OpenWith::Default, None, None).unwrap();
}

/// The UI's fallback for a refused default app is "reveal instead" (`openWithDefaultApp`). For a
/// symlink the reveal is refused too, and that refusal (not a silent no-op) is what the user sees.
#[cfg(unix)]
#[test]
fn a_symlinked_item_refused_by_the_default_app_is_not_revealed_either() {
    use std::os::unix::fs::symlink;
    let d = wc();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), b"x").unwrap();
    symlink(
        outside.path().join("secret.txt"),
        d.path().join("notes.txt"),
    )
    .unwrap();
    symlink(outside.path(), d.path().join("docs")).unwrap();
    std::fs::write(d.path().join("inside.txt"), b"x").unwrap();
    symlink("inside.txt", d.path().join("alias.txt")).unwrap();
    let r = Recorder::default();
    // A link through a folder that leaves the working copy is refused before the default-app
    // check, without offering the reveal fallback at all.
    let escaped = open_external(
        &r,
        d.path(),
        "docs/secret.txt",
        OpenWith::Default,
        None,
        None,
    )
    .unwrap_err();
    assert_eq!(escaped.actions, None, "{}", escaped.message);
    for n in ["notes.txt", "docs/secret.txt", "alias.txt"] {
        if n != "docs/secret.txt" {
            let opened = open_external(&r, d.path(), n, OpenWith::Default, None, None).unwrap_err();
            refused_with_reveal(&opened, n);
        }
        let revealed = reveal_in_os(&r, d.path(), n).unwrap_err();
        assert!(
            revealed.message.contains("symbolic link"),
            "{n}: {}",
            revealed.message
        );
        assert_eq!(revealed.actions, None, "{n}: no second fallback is offered");
    }
    assert!(r.calls.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn an_execute_bit_also_blocks_the_default_app() {
    use std::os::unix::fs::PermissionsExt;
    let d = wc();
    let f = d.path().join("tool.txt");
    std::fs::write(&f, b"binary").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!default_app_allowed(&f));
    assert!(default_app_allowed(&d.path().join("README.MD")));
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

/// A versioned symlink (`svn:special`) may point anywhere: neither a link as the last component
/// nor one in the middle of the path reaches the editor or the terminal, also when it points
/// inside the working copy (same rule as reveal).
#[cfg(unix)]
#[test]
fn editor_and_terminal_refuse_symlinks_inside_the_working_copy() {
    use std::os::unix::fs::symlink;
    let d = wc();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), b"s").unwrap();
    symlink(outside.path().join("secret.txt"), d.path().join("notes.txt")).unwrap();
    symlink(outside.path(), d.path().join("docs")).unwrap();
    symlink("src", d.path().join("alias")).unwrap();
    let r = Recorder::default();
    let editor = tool("/usr/bin/code", &[]);
    let terminal = tool("/usr/bin/term", &[]);
    for rel in ["notes.txt", "docs", "docs/secret.txt", "alias", "alias/main.rs"] {
        for with in [OpenWith::Editor, OpenWith::Terminal] {
            let err = open_external(&r, d.path(), rel, with, Some(&editor), Some(&terminal))
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgument, "{rel} {with:?}");
            assert!(err.message.contains("symbolic link"), "{rel}: {}", err.message);
            assert_eq!(err.path.as_deref(), Some(rel));
        }
    }
    assert!(r.calls.lock().unwrap().is_empty(), "nothing was launched");
}

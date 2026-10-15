#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Word and Excel diffs as extracted text, through the commands and the fake engine. The
//! documents are built here from hand-written parts (namespaces on the reserved `example.com`).

use std::time::Duration;

use opensvn_ipc::{CommitRequest, DiffGetRequest, DiffSideKind, DiffTarget, OfficeKind, Rev, WcId};

use super::fixture::Fixture;
use crate::commands;
use crate::diagnostics::zip::write_zip;
use crate::engine::fake::{DEMO_REPO_URL, demo_wc_root};
use crate::engine::{EngineFacade, OpContext, WcRef};

fn docx(paragraphs: &[&str], picture_bytes: usize) -> Vec<u8> {
    let body: String = paragraphs
        .iter()
        .map(|p| format!("<w:p><w:r><w:t>{p}</w:t></w:r></w:p>"))
        .collect();
    let doc = format!(
        r#"<w:document xmlns:w="http://example.com/w"><w:body>{body}</w:body></w:document>"#
    );
    write_zip(&[
        ("word/document.xml".to_owned(), doc.into_bytes()),
        (
            "word/media/image1.png".to_owned(),
            vec![0x89; picture_bytes],
        ),
    ])
    .unwrap()
}

fn request(target: DiffTarget) -> DiffGetRequest {
    DiffGetRequest {
        target,
        ignore_whitespace: None,
        force_large: None,
    }
}

/// Commits `report.docx` with `content` into the demo working copy; returns the new revision.
async fn commit_docx(f: &Fixture, wc_id: &WcId, content: &[u8]) -> Rev {
    let root = demo_wc_root();
    f.engine.edit_file(&root, "report.docx", content).unwrap();
    let wc = WcRef {
        id: wc_id.clone(),
        root,
    };
    f.engine
        .commit(
            &wc,
            &CommitRequest {
                wc_id: wc_id.clone(),
                paths: vec!["report.docx".into()],
                message: "Report".into(),
                keep_locks: false,
                auto_add_remove: true,
            },
            &OpContext::detached(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn word_files_are_compared_as_their_text_in_the_working_copy_and_in_history() {
    let f = Fixture::new(Duration::ZERO).await;
    let wc = f.add_demo_wc().await;
    // Pictures make the files far larger than the inline text limit; only the text is read.
    let big = 6 * 1024 * 1024;
    let r1 = commit_docx(&f, &wc.id, &docx(&["Điều 1", "Giá 100"], big)).await;
    let r2 = commit_docx(&f, &wc.id, &docx(&["Điều 1", "Giá 120"], big + 1)).await;
    f.engine
        .edit_file(
            &demo_wc_root(),
            "report.docx",
            &docx(&["Điều 1", "Giá 150", "Mới"], 10),
        )
        .unwrap();

    let d = commands::diff_get(
        f.state(),
        request(DiffTarget::Wc {
            wc_id: wc.id.clone(),
            path: "report.docx".into(),
        }),
    )
    .await
    .unwrap();
    let office = d.office.clone().unwrap();
    assert!(office.extracted && office.kind == OfficeKind::Word);
    assert!(!d.binary);
    let (left, right) = (d.left.unwrap(), d.right.unwrap());
    assert_eq!(left.content, "Điều 1\nGiá 120\n");
    assert_eq!(right.content, "Điều 1\nGiá 150\nMới\n");
    assert_eq!(left.label, "report.docx (base)");

    let url = format!("{DEMO_REPO_URL}/trunk/report.docx");
    let d = commands::diff_get(
        f.state(),
        request(DiffTarget::Rev {
            url: url.clone(),
            rev: r2,
        }),
    )
    .await
    .unwrap();
    assert_eq!(d.left.unwrap().content, "Điều 1\nGiá 100\n");
    assert_eq!(d.right.unwrap().content, "Điều 1\nGiá 120\n");
    assert!(d.office.unwrap().extracted);

    // The revision that added the file: one-sided.
    let d = commands::diff_get(f.state(), request(DiffTarget::Rev { url, rev: r1 }))
        .await
        .unwrap();
    assert!(d.left.is_none());
    assert_eq!(d.right.unwrap().content, "Điều 1\nGiá 100\n");

    // Nothing is left behind in the private folder.
    let leftovers = std::fs::read_dir(f.state().data_dir.join(crate::preview::OFFICE_DIFF_DIR))
        .unwrap()
        .count();
    assert_eq!(leftovers, 0);
}

#[tokio::test]
async fn unreadable_and_legacy_office_files_explain_why_there_is_no_text() {
    let f = Fixture::new(Duration::ZERO).await;
    let wc = f.add_demo_wc().await;
    let root = demo_wc_root();
    f.engine
        .edit_file(&root, "old.doc", b"\xD0\xCF\x11\xE0 legacy")
        .unwrap();
    f.engine
        .edit_file(&root, "broken.xlsx", b"not a zip")
        .unwrap();
    let wc_ref = WcRef {
        id: wc.id.clone(),
        root: root.clone(),
    };
    f.engine
        .add_paths(
            &wc_ref,
            &["old.doc".to_owned(), "broken.xlsx".to_owned()],
            &OpContext::detached(),
        )
        .await
        .unwrap();
    for (path, kind, why) in [
        ("old.doc", OfficeKind::Word, "Word 97-2003"),
        (
            "broken.xlsx",
            OfficeKind::Excel,
            "not an Office Open XML file",
        ),
    ] {
        let d = commands::diff_get(
            f.state(),
            request(DiffTarget::Wc {
                wc_id: wc.id.clone(),
                path: path.into(),
            }),
        )
        .await
        .unwrap();
        let office = d.office.unwrap();
        assert!(d.binary && !office.extracted && office.kind == kind);
        assert!(office.reason.unwrap().contains(why), "{path}");
    }
}

#[tokio::test]
async fn each_version_opens_read_only_in_the_default_app() {
    let f = Fixture::new(Duration::ZERO).await;
    let wc = f.add_demo_wc().await;
    let v1 = docx(&["một"], 100);
    let v2 = docx(&["hai"], 100);
    let r1 = commit_docx(&f, &wc.id, &v1).await;
    let r2 = commit_docx(&f, &wc.id, &v2).await;
    let url = format!("{DEMO_REPO_URL}/trunk/report.docx");
    let opened = || {
        let calls = f.opener.calls.lock().unwrap();
        let last = calls.last().unwrap().clone();
        std::path::PathBuf::from(last.strip_prefix("open ").unwrap())
    };

    let rev = DiffTarget::Rev {
        url: url.clone(),
        rev: r2,
    };
    commands::diff_open_version(f.state(), rev.clone(), DiffSideKind::Left)
        .await
        .unwrap();
    let left = opened();
    assert_eq!(
        left.file_name().unwrap().to_string_lossy(),
        format!("r{}.docx", r1.0)
    );
    assert!(left.starts_with(&f.state().data_dir));
    assert_eq!(std::fs::read(&left).unwrap(), v1);
    assert!(std::fs::metadata(&left).unwrap().permissions().readonly());

    commands::diff_open_version(f.state(), rev, DiffSideKind::Right)
        .await
        .unwrap();
    assert_eq!(std::fs::read(opened()).unwrap(), v2);

    let wc_target = DiffTarget::Wc {
        wc_id: wc.id.clone(),
        path: "report.docx".into(),
    };
    commands::diff_open_version(f.state(), wc_target, DiffSideKind::Right)
        .await
        .unwrap();
    assert!(opened().ends_with("working.docx"));

    // The revision that added the file has no older version.
    let err = commands::diff_open_version(
        f.state(),
        DiffTarget::Rev { url, rev: r1 },
        DiffSideKind::Left,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, opensvn_ipc::ErrorCode::InvalidArgument);

    // Types the default app may not open (macros, legacy formats) are refused before any work.
    let before = f.opener.calls.lock().unwrap().len();
    for path in ["old.doc", "book.xlsm", "run.exe"] {
        let err = commands::diff_open_version(
            f.state(),
            DiffTarget::Wc {
                wc_id: wc.id.clone(),
                path: path.into(),
            },
            DiffSideKind::Right,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, opensvn_ipc::ErrorCode::InvalidArgument, "{path}");
    }
    assert_eq!(f.opener.calls.lock().unwrap().len(), before);
}

#[tokio::test]
async fn a_diff_tool_set_for_the_extension_gets_both_files_even_when_binary() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new(Duration::ZERO).await;
    let wc = f.add_demo_wc().await;
    let v1 = docx(&["một"], 50);
    let r1 = commit_docx(&f, &wc.id, &v1).await;
    let v2 = docx(&["hai"], 60);
    f.engine
        .edit_file(&demo_wc_root(), "report.docx", &v2)
        .unwrap();
    let target = DiffTarget::Wc {
        wc_id: wc.id.clone(),
        path: "report.docx".into(),
    };

    let dir = tempfile::tempdir().unwrap();
    let exe = dir
        .path()
        .join(if cfg!(windows) { "cmp.exe" } else { "cmp" });
    std::fs::write(&exe, b"x").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    *f.picker.pick.lock().unwrap() = Some(exe.clone());
    f.picker.accept.store(true, Ordering::SeqCst);
    commands::program_choose_for_extension(
        f.state(),
        "docx".into(),
        vec!["{left}".into(), "{right}".into()],
    )
    .await
    .unwrap();

    commands::open_external_diff(&f.state(), target)
        .await
        .unwrap();
    let launched = f.launcher.launched.lock().unwrap().clone();
    assert_eq!(launched.len(), 1);
    let (program, args) = &launched[0];
    assert_eq!(program, &std::fs::canonicalize(&exe).unwrap());
    let (l, r) = (
        std::path::Path::new(&args[0]),
        std::path::Path::new(&args[1]),
    );
    assert!(l.ends_with("left.docx") && r.ends_with("right.docx"));
    assert_eq!(std::fs::read(l).unwrap(), v1);
    assert_eq!(std::fs::read(r).unwrap(), v2);
    assert!(std::fs::metadata(r).unwrap().permissions().readonly());

    // In history, the revision that added the file: the missing older side is an empty file.
    commands::open_external_diff(
        &f.state(),
        DiffTarget::Rev {
            url: format!("{DEMO_REPO_URL}/trunk/report.docx"),
            rev: r1,
        },
    )
    .await
    .unwrap();
    let launched = f.launcher.launched.lock().unwrap().clone();
    let args = &launched[1].1;
    assert_eq!(std::fs::read(&args[0]).unwrap(), b"");
    assert_eq!(std::fs::read(&args[1]).unwrap(), v1);

    // Without a tool for the extension, binary files are still refused by the global tool path.
    commands::program_clear_for_extension(f.state(), ".docx".into())
        .await
        .unwrap();
    let err = commands::open_external_diff(
        &f.state(),
        DiffTarget::Wc {
            wc_id: wc.id.clone(),
            path: "report.docx".into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, opensvn_ipc::ErrorCode::InvalidArgument);
}

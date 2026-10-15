#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Word and Excel diffs as extracted text, through the commands and the fake engine. The
//! documents are built here from hand-written parts (namespaces on the reserved `example.com`).

use std::time::Duration;

use opensvn_ipc::{CommitRequest, DiffGetRequest, DiffTarget, OfficeKind, Rev, WcId};

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

//! `export_wc_sides` / `export_rev_sides`: both sides streamed to files whatever their size.

use std::sync::Arc;

use bytes::Bytes;
use opensvn_ipc::{Rev, WcId};
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_types::{CancelToken, Depth, ErrorCode, RelPath};

use super::support::{StaticConnector, URL, ctx, fixture, fixture_on, put};
use crate::engine::real::ClientRecovery;
use crate::engine::{EngineFacade, SideFiles, WcRef};

/// More than the 5 MiB the inline diff reads.
const BIG: usize = 6 * 1024 * 1024 + 7;

fn binary(path: &str, fill: u8, len: usize) -> FakeChange {
    FakeChange::PutFile(RelPath::new(path).unwrap(), Bytes::from(vec![fill; len]))
}

#[tokio::test]
async fn revision_sides_of_any_size_are_written_whole() {
    let f = fixture();
    f.repo
        .seed("bob", "Add", &[binary("trunk/report.docx", 1, BIG)])
        .unwrap();
    f.repo
        .seed("bob", "Edit", &[binary("trunk/report.docx", 2, BIG + 1)])
        .unwrap();
    let url = format!("{URL}/trunk/report.docx");
    let dir = tempfile::tempdir().unwrap();
    let (l, r) = (dir.path().join("l"), dir.path().join("r"));
    let to = SideFiles {
        left: Some(&l),
        right: Some(&r),
    };
    let sides = f
        .engine
        .export_rev_sides(&url, None, Rev(3), to, &ctx())
        .await
        .unwrap();
    assert_eq!(sides.left.as_deref(), Some("report.docx@r2"));
    assert_eq!(sides.right.as_deref(), Some("report.docx@r3"));
    assert_eq!(std::fs::read(&l).unwrap(), vec![1; BIG]);
    assert_eq!(std::fs::read(&r).unwrap(), vec![2; BIG + 1]);

    // The revision that added the file has no left side, and no left file is left behind.
    let (l2, r2) = (dir.path().join("l2"), dir.path().join("r2"));
    let sides = f
        .engine
        .export_rev_sides(
            &url,
            None,
            Rev(2),
            SideFiles {
                left: Some(&l2),
                right: Some(&r2),
            },
            &ctx(),
        )
        .await
        .unwrap();
    assert!(sides.left.is_none() && !l2.exists());
    assert_eq!(std::fs::metadata(&r2).unwrap().len(), BIG as u64);

    // Only the requested side is written; an existing destination is never overwritten.
    let only = dir.path().join("only");
    let sides = f
        .engine
        .export_rev_sides(
            &url,
            Some(Rev(2)),
            Rev(3),
            SideFiles {
                left: None,
                right: Some(&only),
            },
            &ctx(),
        )
        .await
        .unwrap();
    assert!(sides.left.is_none() && sides.right.is_some());
    let again = f
        .engine
        .export_rev_sides(
            &url,
            None,
            Rev(3),
            SideFiles {
                left: None,
                right: Some(&only),
            },
            &ctx(),
        )
        .await;
    assert!(again.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn working_copy_sides_are_the_pristine_text_and_the_file_on_disk() {
    // The in-memory repository sends a file as one delta window, which a checkout caps; the
    // copies themselves are streamed whatever the size.
    const BIG: usize = 90 * 1024 + 7;
    let repo = FakeRaSession::new("https://svn.test.local/repos/flat", "flat-uuid").unwrap();
    repo.seed(
        "alice",
        "Import",
        &[put("notes.txt", "v1\n"), binary("deck.xlsx", 3, BIG)],
    )
    .unwrap();
    repo.seed("alice", "Edit", &[put("notes.txt", "v2\n")])
        .unwrap();
    let connector = Arc::new(StaticConnector(vec![repo.clone()]));
    let f = fixture_on(repo, connector, Arc::new(ClientRecovery));
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wc");
    svn_client::checkout(
        &f.engine.client_ctx(&ctx()),
        &f.repo,
        &root,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    let wc = WcRef {
        id: WcId("wc".into()),
        root: root.canonicalize().unwrap(),
    };
    std::fs::write(root.join("deck.xlsx"), vec![4; BIG + 3]).unwrap();
    std::fs::write(root.join("notes.txt"), "local\r\n").unwrap();
    std::fs::write(root.join("new.docx"), "unversioned").unwrap();

    let out = tempfile::tempdir().unwrap();
    let path = |n: &str| out.path().join(n);
    let sides = f
        .engine
        .export_wc_sides(
            &wc,
            "deck.xlsx",
            None,
            SideFiles {
                left: Some(&path("a")),
                right: Some(&path("b")),
            },
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(sides.left.as_deref(), Some("deck.xlsx (base)"));
    assert_eq!(sides.right.as_deref(), Some("deck.xlsx (working)"));
    assert_eq!(std::fs::read(path("a")).unwrap(), vec![3; BIG]);
    assert_eq!(std::fs::read(path("b")).unwrap(), vec![4; BIG + 3]);

    // Against a revision: that revision from the repository; the working file byte for byte.
    let sides = f
        .engine
        .export_wc_sides(
            &wc,
            "notes.txt",
            Some(Rev(1)),
            SideFiles {
                left: Some(&path("c")),
                right: Some(&path("d")),
            },
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(sides.left.as_deref(), Some("notes.txt@r1"));
    assert_eq!(std::fs::read(path("c")).unwrap(), b"v1\n");
    assert_eq!(std::fs::read(path("d")).unwrap(), b"local\r\n");

    // An unversioned file has a working side only; a directory is refused.
    let sides = f
        .engine
        .export_wc_sides(
            &wc,
            "new.docx",
            None,
            SideFiles {
                left: Some(&path("e")),
                right: Some(&path("f")),
            },
            &ctx(),
        )
        .await
        .unwrap();
    assert!(sides.left.is_none() && !path("e").exists());
    assert_eq!(std::fs::read(path("f")).unwrap(), b"unversioned");
    std::fs::create_dir(root.join("folder")).unwrap();
    let err = f
        .engine
        .export_wc_sides(
            &wc,
            "folder",
            None,
            SideFiles {
                left: Some(&path("g")),
                right: None,
            },
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::WcInvalidOperation);
}

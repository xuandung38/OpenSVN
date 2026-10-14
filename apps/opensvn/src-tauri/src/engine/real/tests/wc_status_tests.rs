//! Status, working copy diff and remote status of the real engine on working copies checked out
//! by `svn-client` from the in-memory repository, compared with the fake engine on the same tree.
//!
//! The scenarios mirror the status, diff and conflict parts of `engine::conformance`. They cannot
//! run the shared suite itself yet: every scenario starts with a facade checkout (not wired until
//! the sync package lands), and sessions from `StaticConnector` are rooted at the repository
//! root, which `svn-client` accepts only for a working copy of the root. So the repository here
//! is "flat" (the standard files at the root, no `trunk`), the working copies are checked out with
//! `svn_client::checkout`, and local changes that need operations not wired yet are made with
//! `svn-client` directly.

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use opensvn_ipc::{
    FileDiff, FileKind, FileStatus, IgnoreAddRequest, IgnoreScope, NodeStatus, PropStatus, Rev,
    UpdateRequest, WcFormat, WcId,
};
use svn_ra_api::RaSession;
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_types::{CancelToken, Depth, ErrorCode, PropChange, RelPath};

use super::support::{Fixture, StaticConnector, ctx, fixture_on, put};
use crate::engine::fake::FakeEngine;
use crate::engine::real::ClientRecovery;
use crate::engine::real::wc_status::INCOMING_CAP;
use crate::engine::{
    CheckoutPlan, DiffOptions, EngineFacade, LockScope, OpContext, StatusQuery, WcRef,
};

const FLAT_URL: &str = "https://svn.test.local/repos/flat";

const ALL: StatusQuery = StatusQuery {
    include_ignored: false,
    include_unversioned: true,
};

/// Repository with the standard files at its root (revision 1).
fn flat_repo() -> FakeRaSession {
    let repo = FakeRaSession::new(FLAT_URL, "flat-uuid").unwrap();
    repo.seed(
        "alice",
        "Initial import",
        &[
            put("README.md", "hello\n"),
            put("src/main.rs", "fn main() {}\n"),
        ],
    )
    .unwrap();
    repo
}

fn set_prop(path: &str, name: &str, value: &str) -> FakeChange {
    FakeChange::SetProp(
        RelPath::new(path).unwrap(),
        PropChange {
            name: name.to_owned(),
            value: Some(Bytes::from(value.to_owned())),
        },
    )
}

/// Real engine over a flat repository plus a directory for its working copies.
struct Real {
    f: Fixture,
    dir: tempfile::TempDir,
}

impl Real {
    fn new() -> Self {
        let repo = flat_repo();
        let connector = Arc::new(StaticConnector(vec![repo.clone()]));
        Self {
            f: fixture_on(repo, connector, Arc::new(ClientRecovery)),
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn engine(&self) -> &dyn EngineFacade {
        &self.f.engine
    }

    async fn checkout(&self, name: &str) -> WcRef {
        let root = self.dir.path().join(name);
        svn_client::checkout(
            &self.f.engine.client_ctx(&ctx()),
            &self.f.repo,
            &root,
            None,
            Depth::Infinity,
            false,
            &CancelToken::new(),
        )
        .await
        .expect("checkout");
        WcRef {
            id: WcId(name.to_owned()),
            root: root.canonicalize().unwrap(),
        }
    }

    async fn update(&self, wc: &WcRef) {
        svn_client::update(
            &self.f.engine.client_ctx(&ctx()),
            &self.f.repo,
            &wc.root,
            None,
            Depth::Infinity,
            &CancelToken::new(),
        )
        .await
        .expect("update");
    }

    async fn status(&self, wc: &WcRef) -> Vec<FileStatus> {
        self.engine().status(wc, ALL, &ctx()).await.expect("status")
    }

    async fn diff(&self, wc: &WcRef, path: &str, against: Option<Rev>) -> FileDiff {
        self.engine()
            .diff_wc(wc, path, against, DiffOptions::default(), &ctx())
            .await
            .expect("diff_wc")
    }

    fn client_db(&self, wc: &WcRef) -> svn_wc::WcDb {
        svn_client::wc::open_containing(&self.f.engine.client_ctx(&ctx()).lock_owner, &wc.root)
            .unwrap()
            .0
    }

    fn add(&self, wc: &WcRef, path: &str) {
        svn_client::local::add(
            &self.client_db(wc),
            &CancelToken::new(),
            &[wc.root.join(path)],
            &svn_client::local::AddOptions::default(),
            &svn_client::local::AddConfig::default(),
        )
        .unwrap();
    }

    fn delete(&self, wc: &WcRef, path: &str) {
        svn_client::local::delete(
            &self.client_db(wc),
            &CancelToken::new(),
            &[wc.root.join(path)],
            &svn_client::local::DeleteOptions::default(),
        )
        .unwrap();
    }

    fn propset(&self, wc: &WcRef, path: &str, name: &str, value: &str) {
        svn_client::local::propset(
            &self.client_db(wc),
            &CancelToken::new(),
            &RelPath::new(path).unwrap(),
            name,
            Some(value.as_bytes()),
            svn_client::local::PropSetOptions::default(),
        )
        .unwrap();
    }

    fn changelist(&self, wc: &WcRef, path: &str, name: &str) {
        svn_client::local::set_changelist(
            &self.client_db(wc),
            &CancelToken::new(),
            &[wc.root.join(path)],
            Some(name),
            Depth::Empty,
        )
        .unwrap();
    }
}

fn write(wc: &WcRef, rel: &str, content: &[u8]) {
    let path = wc.root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// Fake engine over its own flat repository.
struct Fake {
    engine: FakeEngine,
    repo: FakeRaSession,
}

impl Fake {
    fn new() -> Self {
        let repo = flat_repo();
        Self {
            engine: FakeEngine::empty().with_repo(repo.clone()),
            repo,
        }
    }

    async fn checkout(&self, name: &str) -> WcRef {
        let base = if cfg!(windows) {
            PathBuf::from(r"C:\opensvn-fake\work")
        } else {
            PathBuf::from("/opensvn-fake/work")
        };
        let info = self
            .engine
            .checkout(
                &CheckoutPlan {
                    url: FLAT_URL.to_owned(),
                    path: base.join(name),
                    rev: None,
                    depth: opensvn_ipc::Depth::Infinity,
                    sparse: None,
                    format: WcFormat::V31,
                    store_pristine: true,
                    ignore_externals: false,
                },
                &ctx(),
            )
            .await
            .expect("fake checkout");
        WcRef {
            id: WcId(name.to_owned()),
            root: PathBuf::from(info.root_path),
        }
    }

    fn write(&self, wc: &WcRef, rel: &str, content: &[u8]) {
        self.engine.edit_file(&wc.root, rel, content).unwrap();
    }

    async fn status(&self, wc: &WcRef) -> Vec<FileStatus> {
        self.engine.status(wc, ALL, &ctx()).await.expect("status")
    }

    async fn diff(&self, wc: &WcRef, path: &str, against: Option<Rev>) -> FileDiff {
        self.engine
            .diff_wc(wc, path, against, DiffOptions::default(), &ctx())
            .await
            .expect("diff_wc")
    }
}

fn find<'a>(statuses: &'a [FileStatus], path: &str) -> &'a FileStatus {
    statuses
        .iter()
        .find(|s| s.path == path)
        .unwrap_or_else(|| panic!("{path} not in {statuses:?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn edits_show_in_status_and_diff_like_the_fake_engine() {
    let (real, fake) = (Real::new(), Fake::new());
    let (rw, fw) = (real.checkout("edit").await, fake.checkout("edit").await);
    assert!(real.status(&rw).await.is_empty(), "fresh checkout is clean");
    assert!(fake.status(&fw).await.is_empty());

    write(&rw, "README.md", b"hello world\n");
    write(&rw, "notes.txt", b"todo\n");
    fake.write(&fw, "README.md", b"hello world\n");
    fake.write(&fw, "notes.txt", b"todo\n");

    let st = real.status(&rw).await;
    assert_eq!(find(&st, "README.md").text, NodeStatus::Modified);
    assert_eq!(find(&st, "notes.txt").text, NodeStatus::Unversioned);
    assert_eq!(st, fake.status(&fw).await);

    let diff = real.diff(&rw, "README.md", None).await;
    assert_eq!(diff.left.as_ref().unwrap().content, "hello\n");
    assert_eq!(diff.right.as_ref().unwrap().content, "hello world\n");
    assert!(!diff.binary && diff.too_large.is_none() && diff.prop_changes.is_empty());
    assert_eq!(diff, fake.diff(&fw, "README.md", None).await);

    // An unversioned file is a one-sided addition of what is on disk, like the fake engine.
    let unversioned = real.diff(&rw, "notes.txt", None).await;
    assert!(unversioned.left.is_none());
    assert_eq!(unversioned.right.as_ref().unwrap().content, "todo\n");
    assert_eq!(unversioned, fake.diff(&fw, "notes.txt", None).await);
    let err = real
        .engine()
        .diff_wc(
            &rw,
            "nothing-here.txt",
            None,
            DiffOptions::default(),
            &ctx(),
        )
        .await
        .expect_err("nothing on disk");
    assert_eq!(err.code, ErrorCode::PathNotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn added_deleted_and_missing_files_have_one_sided_diffs() {
    let (real, fake) = (Real::new(), Fake::new());
    let (rw, fw) = (real.checkout("sched").await, fake.checkout("sched").await);

    write(&rw, "new.txt", b"new\n");
    real.add(&rw, "new.txt");
    fake.write(&fw, "new.txt", b"new\n");
    fake.engine
        .add_paths(&fw, &["new.txt".into()], &ctx())
        .await
        .unwrap();
    real.delete(&rw, "README.md");
    fake.engine
        .delete_paths(&fw, &["README.md".into()], false, &ctx())
        .await
        .unwrap();

    let st = real.status(&rw).await;
    assert_eq!(find(&st, "new.txt").text, NodeStatus::Added);
    let readme = find(&st, "README.md");
    assert_eq!(readme.text, NodeStatus::Deleted);
    assert_eq!(readme.size, None, "a deleted file has no size");
    assert_eq!(
        st,
        fake.status(&fw).await,
        "deleted files have no size in both engines"
    );

    let added = real.diff(&rw, "new.txt", None).await;
    assert!(added.left.is_none());
    assert_eq!(added.right.as_ref().unwrap().content, "new\n");
    assert_eq!(added, fake.diff(&fw, "new.txt", None).await);

    let deleted = real.diff(&rw, "README.md", None).await;
    assert_eq!(deleted.left.as_ref().unwrap().content, "hello\n");
    assert!(deleted.right.is_none());
    assert_eq!(deleted, fake.diff(&fw, "README.md", None).await);

    std::fs::remove_file(rw.root.join("src/main.rs")).unwrap();
    assert_eq!(
        find(&real.status(&rw).await, "src/main.rs").text,
        NodeStatus::Missing
    );
    let missing = real.diff(&rw, "src/main.rs", None).await;
    assert_eq!(missing.left.unwrap().content, "fn main() {}\n");
    assert!(missing.right.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn locked_and_changelisted_unmodified_files_are_listed() {
    let (real, fake) = (Real::new(), Fake::new());
    let (rw, fw) = (real.checkout("meta").await, fake.checkout("meta").await);
    for (engine, wc) in [
        (real.engine(), &rw),
        (&fake.engine as &dyn EngineFacade, &fw),
    ] {
        engine
            .lock(
                &LockScope::Wc {
                    wc: wc.clone(),
                    paths: vec!["README.md".into()],
                },
                Some("editing"),
                false,
                &ctx(),
            )
            .await
            .expect("lock");
    }
    real.changelist(&rw, "src/main.rs", "docs");
    fake.engine
        .changelist_set(&fw, &["src/main.rs".into()], Some("docs"), &ctx())
        .await
        .unwrap();

    let st = real.status(&rw).await;
    let readme = find(&st, "README.md");
    assert_eq!(readme.text, NodeStatus::Normal);
    let lock = readme.lock.as_ref().expect("lock in status");
    assert!(lock.mine);
    assert_eq!(lock.token, None, "the lock token never reaches the UI");
    assert_eq!(lock.comment.as_deref(), Some("editing"));
    let main = find(&st, "src/main.rs");
    assert_eq!(
        (main.text, main.changelist.as_deref()),
        (NodeStatus::Normal, Some("docs"))
    );
    // Lock dates differ between the two repositories; everything else matches.
    let same_dates = |mut v: Vec<FileStatus>| {
        for s in &mut v {
            if let Some(l) = &mut s.lock {
                l.created_at.clear();
            }
        }
        v
    };
    let fake_st = fake.status(&fw).await;
    assert_eq!(
        find(&fake_st, "README.md").lock.as_ref().unwrap().token,
        None,
        "neither engine sends lock tokens over IPC"
    );
    assert_eq!(same_dates(st), same_dates(fake_st));
}

#[tokio::test(flavor = "multi_thread")]
async fn property_changes_are_diffed_for_files_and_directories() {
    let (real, fake) = (Real::new(), Fake::new());
    let (rw, fw) = (real.checkout("props").await, fake.checkout("props").await);
    real.propset(&rw, "README.md", "x:color", "blue");
    fake.engine
        .props_set(&fw, "README.md", "x:color", Some("blue"), &ctx())
        .await
        .unwrap();

    let st = real.status(&rw).await;
    assert_eq!(find(&st, "README.md").props, PropStatus::Modified);
    assert_eq!(st, fake.status(&fw).await);

    let diff = real.diff(&rw, "README.md", None).await;
    assert_eq!(diff.prop_changes.len(), 1);
    let change = &diff.prop_changes[0];
    assert_eq!(
        (
            change.name.as_str(),
            change.before.as_deref(),
            change.after.as_deref()
        ),
        ("x:color", None, Some("blue"))
    );
    assert_eq!(diff.left.as_ref().unwrap().content, "hello\n");
    assert_eq!(diff.right.as_ref().unwrap().content, "hello\n");

    real.propset(&rw, "src", "x:owner", "team");
    fake.engine
        .props_set(&fw, "src", "x:owner", Some("team"), &ctx())
        .await
        .unwrap();
    let dir = real.diff(&rw, "src", None).await;
    assert!(dir.left.is_none() && dir.right.is_none() && !dir.binary);
    assert_eq!(dir.prop_changes[0].name, "x:owner");
    assert_eq!(dir, fake.diff(&fw, "src", None).await, "property-only diff");
    for (engine, wc) in [
        (real.engine(), &rw),
        (&fake.engine as &dyn EngineFacade, &fw),
    ] {
        let err = engine
            .diff_wc(wc, "src", Some(Rev(1)), DiffOptions::default(), &ctx())
            .await
            .expect_err("directories are not compared with revisions");
        assert_eq!(err.code, ErrorCode::WcInvalidOperation);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_root_is_listed_with_its_svn_ignore_property_like_the_fake_engine() {
    let (real, fake) = (Real::new(), Fake::new());
    let (rw, fw) = (real.checkout("root").await, fake.checkout("root").await);
    real.propset(&rw, "", "svn:ignore", "*.log\n");
    fake.engine
        .ignore_add(
            &fw,
            &IgnoreAddRequest {
                wc_id: fw.id.clone(),
                pattern: "*.log".into(),
                dir: String::new(),
                scope: IgnoreScope::SvnIgnore,
            },
            &ctx(),
        )
        .await
        .unwrap();
    write(&rw, "build.log", b"noise\n");
    fake.write(&fw, "build.log", b"noise\n");

    let st = real.status(&rw).await;
    let root = find(&st, "");
    assert_eq!(
        (root.kind, root.text, root.props),
        (FileKind::Dir, NodeStatus::Normal, PropStatus::Modified)
    );
    assert!(
        st.iter().all(|s| s.path != "build.log"),
        "ignored by svn:ignore"
    );
    assert_eq!(st, fake.status(&fw).await);

    let diff = real.diff(&rw, "", None).await;
    let rows: Vec<_> = diff
        .prop_changes
        .iter()
        .map(|c| (c.name.as_str(), c.before.as_deref(), c.after.as_deref()))
        .collect();
    assert_eq!(rows, vec![("svn:ignore", None, Some("*.log\n"))]);
    assert_eq!(diff, fake.diff(&fw, "", None).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn unversioned_files_follow_the_cap_and_binary_rules() {
    let real = Real::new();
    let rw = real.checkout("unv").await;
    write(&rw, "blob.bin", b"\0\x01\x02binary");
    let bin = real.diff(&rw, "blob.bin", None).await;
    assert!(bin.binary && bin.left.is_none() && bin.right.is_none());

    let big = "y".repeat(super::super::remote::CAT_LIMIT + 1);
    write(&rw, "big.txt", big.as_bytes());
    let large = real.diff(&rw, "big.txt", None).await;
    assert_eq!(large.too_large, Some(true));
    assert!(large.left.is_none() && large.right.is_none() && large.hunks.is_none());
    let forced = real
        .engine()
        .diff_wc(
            &rw,
            "big.txt",
            None,
            DiffOptions {
                force_large: true,
                full_text: true,
                ..DiffOptions::default()
            },
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(forced.right.unwrap().content.len(), big.len());

    // Exactly at the cap is still shown.
    let at_cap = "z".repeat(super::super::remote::CAT_LIMIT);
    write(&rw, "at-cap.txt", at_cap.as_bytes());
    let shown = real
        .engine()
        .diff_wc(
            &rw,
            "at-cap.txt",
            None,
            DiffOptions {
                full_text: true,
                ..DiffOptions::default()
            },
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(shown.too_large, None);
    assert_eq!(shown.right.unwrap().content.len(), at_cap.len());

    let err = real
        .engine()
        .diff_wc(
            &rw,
            "blob.bin",
            Some(Rev(1)),
            DiffOptions::default(),
            &ctx(),
        )
        .await
        .expect_err("an unversioned file has no repository side");
    assert_eq!(err.code, ErrorCode::PathNotFound);

    // An unversioned folder has nothing to show but is not an error.
    std::fs::create_dir(rw.root.join("newdir")).unwrap();
    let dir = real.diff(&rw, "newdir", None).await;
    assert!(dir.left.is_none() && dir.right.is_none() && dir.prop_changes.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_paths_never_escape_the_working_copy() {
    let real = Real::new();
    let rw = real.checkout("escape").await;
    std::fs::write(real.dir.path().join("secret.txt"), b"outside\n").unwrap();
    let outside = real.dir.path().join("secret.txt");
    for path in [
        "../secret.txt",
        "src/../../secret.txt",
        outside.to_str().unwrap(),
        ".svn/wc.db",
        "src/.svn/entries",
    ] {
        let err = real
            .engine()
            .diff_wc(&rw, path, None, DiffOptions::default(), &ctx())
            .await
            .expect_err(path);
        assert_ne!(err.code, ErrorCode::Cancelled, "{path}");
    }

    // Spellings that some file system resolves to the administrative folder are refused even
    // where they are distinct names (here on disk), so their content is never shown.
    for alias in [".svn.", ".svn ", "SVN~1", "_svn", ".SVN"] {
        let dir = rw.root.join("src").join(alias);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("wc.db"), b"admin\n").unwrap();
        let path = format!("src/{alias}/wc.db");
        let err = real
            .engine()
            .diff_wc(&rw, &path, None, DiffOptions::default(), &ctx())
            .await
            .expect_err(&path);
        assert_eq!(err.code, ErrorCode::PathNotFound, "{path}");
    }

    #[cfg(unix)]
    {
        // A symlinked folder pointing outside is not followed.
        std::os::unix::fs::symlink(real.dir.path(), rw.root.join("out")).unwrap();
        let err = real
            .engine()
            .diff_wc(&rw, "out/secret.txt", None, DiffOptions::default(), &ctx())
            .await
            .expect_err("through a symlinked folder");
        assert_eq!(err.code, ErrorCode::WcInvalidOperation);
        // A symlink itself is shown as its target text, not as the file it points at.
        std::os::unix::fs::symlink(&outside, rw.root.join("link.txt")).unwrap();
        let link = real.diff(&rw, "link.txt", None).await;
        let right = link.right.unwrap().content;
        assert!(
            right.starts_with("link ") && !right.contains("outside\n"),
            "{right}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn binary_mime_types_and_large_files_are_sent_without_text() {
    let real = Real::new();
    let rw = real.checkout("big").await;
    real.propset(&rw, "README.md", "svn:mime-type", "application/x-thing");
    write(&rw, "README.md", b"hello again\n");
    let bin = real.diff(&rw, "README.md", None).await;
    assert!(bin.binary);
    assert_eq!(bin.mime.as_deref(), Some("application/x-thing"));
    assert!(bin.left.is_none() && bin.right.is_none());
    assert_eq!(bin.prop_changes[0].name, "svn:mime-type");

    write(&rw, "src/main.rs", b"fn main() {}\n\0\x01binary\n");
    let sniffed = real.diff(&rw, "src/main.rs", None).await;
    assert!(sniffed.binary && sniffed.left.is_none());

    let big = "x".repeat(super::super::remote::CAT_LIMIT + 10);
    write(&rw, "src/main.rs", big.as_bytes());
    let large = real.diff(&rw, "src/main.rs", None).await;
    assert_eq!(large.too_large, Some(true));
    assert!(large.left.is_none() && large.right.is_none() && !large.binary);
    let forced = real
        .engine()
        .diff_wc(
            &rw,
            "src/main.rs",
            None,
            DiffOptions {
                force_large: true,
                full_text: true,
                ..DiffOptions::default()
            },
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(forced.too_large, None);
    assert_eq!(forced.right.unwrap().content.len(), big.len());
    assert_eq!(forced.left.unwrap().content, "fn main() {}\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn working_files_are_compared_in_repository_normal_form() {
    let real = Real::new();
    let rw = real.checkout("eol").await;
    real.propset(&rw, "README.md", "svn:eol-style", "native");
    real.propset(&rw, "README.md", "svn:keywords", "Id");
    write(
        &rw,
        "README.md",
        b"hello\r\n$Id: README.md 1 alice $\r\nmore\r\n",
    );
    let diff = real.diff(&rw, "README.md", None).await;
    assert_eq!(diff.left.unwrap().content, "hello\n");
    assert_eq!(diff.right.unwrap().content, "hello\n$Id$\nmore\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_diff_against_a_revision_reads_the_left_side_from_the_repository() {
    let (real, fake) = (Real::new(), Fake::new());
    let (rw, fw) = (real.checkout("rev").await, fake.checkout("rev").await);
    for repo in [&real.f.repo, &fake.repo] {
        repo.seed(
            "bob",
            "r2",
            &[
                put("README.md", "hello r2\n"),
                set_prop("README.md", "x:k", "v"),
            ],
        )
        .unwrap();
    }
    let diff = real.diff(&rw, "README.md", Some(Rev(2))).await;
    let left = diff.left.as_ref().unwrap();
    assert_eq!(left.content, "hello r2\n");
    assert_eq!(left.label, "README.md@r2");
    assert_eq!(diff.right.as_ref().unwrap().content, "hello\n");
    let rows: Vec<_> = diff
        .prop_changes
        .iter()
        .map(|c| (c.name.as_str(), c.before.as_deref(), c.after.as_deref()))
        .collect();
    assert_eq!(rows, vec![("x:k", Some("v"), None)]);
    let fake_diff = fake.diff(&fw, "README.md", Some(Rev(2))).await;
    assert_eq!(
        (&diff.left, &diff.right, &diff.prop_changes),
        (&fake_diff.left, &fake_diff.right, &fake_diff.prop_changes),
        "both engines send the property rows of a diff against a revision"
    );

    // The binary decision uses the properties of the compared revision too.
    for repo in [&real.f.repo, &fake.repo] {
        repo.seed(
            "bob",
            "r3",
            &[set_prop(
                "README.md",
                "svn:mime-type",
                "application/octet-stream",
            )],
        )
        .unwrap();
    }
    let bin = real.diff(&rw, "README.md", Some(Rev(3))).await;
    assert!(bin.binary && bin.left.is_none() && bin.right.is_none());
    assert_eq!(bin.mime.as_deref(), Some("application/octet-stream"));

    let same = real.diff(&rw, "README.md", Some(Rev(1))).await;
    assert_eq!(same.left.unwrap().content, same.right.unwrap().content);

    write(&rw, "later.txt", b"later\n");
    real.add(&rw, "later.txt");
    let absent = real.diff(&rw, "later.txt", Some(Rev(2))).await;
    assert!(absent.left.is_none());
    assert_eq!(absent.right.unwrap().content, "later\n");

    // A repository side over the cap is not downloaded past it; its properties are unknown.
    let big = "r".repeat(super::super::remote::CAT_LIMIT + 1);
    real.f
        .repo
        .seed("bob", "big", &[put("src/main.rs", &big)])
        .unwrap();
    let head = Rev(4);
    let large = real.diff(&rw, "src/main.rs", Some(head)).await;
    assert_eq!(large.too_large, Some(true));
    assert!(large.left.is_none() && large.right.is_none() && large.hunks.is_none());
    assert!(
        large.prop_changes.is_empty(),
        "no rows for unknown properties"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_too_large_diff_keeps_its_property_rows() {
    let real = Real::new();
    let rw = real.checkout("bigprops").await;
    real.propset(&rw, "src/main.rs", "x:review", "yes");
    let big = "x".repeat(super::super::remote::CAT_LIMIT + 10);
    write(&rw, "src/main.rs", big.as_bytes());
    let diff = real.diff(&rw, "src/main.rs", None).await;
    assert_eq!(diff.too_large, Some(true));
    assert!(diff.left.is_none() && diff.right.is_none() && diff.hunks.is_none());
    let rows: Vec<_> = diff.prop_changes.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(rows, vec!["x:review"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_status_reports_incoming_changes_and_repository_locks() {
    let real = Real::new();
    let rw = real.checkout("remote").await;
    let scan = real
        .engine()
        .status_remote(&rw, &ctx())
        .await
        .expect("status_remote");
    assert_eq!((scan.incoming, scan.latest_rev), (0, Rev(1)));
    assert!(scan.statuses.is_empty());

    real.f
        .repo
        .seed(
            "bob",
            "r2",
            &[
                put("README.md", "hello from bob\n"),
                put("docs/new.txt", "n\n"),
            ],
        )
        .unwrap();
    real.f
        .repo
        .lock(
            &RelPath::new("src/main.rs").unwrap(),
            Some("bob's"),
            false,
            None,
            &CancelToken::new(),
        )
        .await
        .unwrap();

    let scan = real
        .engine()
        .status_remote(&rw, &OpContext::background())
        .await
        .expect("background status_remote");
    assert_eq!((scan.incoming, scan.latest_rev), (1, Rev(2)));
    let readme = find(&scan.statuses, "README.md");
    assert_eq!(readme.text, NodeStatus::Normal);
    let remote = readme.remote.as_ref().unwrap();
    assert!(remote.out_of_date);
    assert_eq!(remote.remote_rev, Some(Rev(2)));
    let main = find(&scan.statuses, "src/main.rs");
    let lock = main.remote.as_ref().unwrap().remote_lock.as_ref().unwrap();
    assert!(!lock.mine && lock.token.is_none());
    assert_eq!(lock.comment.as_deref(), Some("bob's"));
    assert!(!main.remote.as_ref().unwrap().out_of_date);
    // Added in the repository: listed with its remote side, not as an unversioned file.
    let new = scan
        .statuses
        .iter()
        .find(|s| s.path.starts_with("docs"))
        .expect("remote addition");
    assert_eq!(new.text, NodeStatus::Normal);
    assert!(new.remote.as_ref().unwrap().out_of_date);

    real.update(&rw).await;
    let scan = real.engine().status_remote(&rw, &ctx()).await.unwrap();
    assert_eq!((scan.incoming, scan.latest_rev), (0, Rev(2)));
    assert!(
        scan.statuses
            .iter()
            .all(|s| !s.remote.as_ref().unwrap().out_of_date)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_revisions_are_counted_up_to_the_cap() {
    let real = Real::new();
    let rw = real.checkout("many").await;
    for i in 0..5 {
        real.f
            .repo
            .seed("bob", "more", &[put("README.md", &format!("v{i}\n"))])
            .unwrap();
    }
    let scan = real.engine().status_remote(&rw, &ctx()).await.unwrap();
    assert_eq!((scan.incoming, scan.latest_rev), (5, Rev(6)));
    assert!(scan.incoming <= INCOMING_CAP);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_update_conflict_shows_in_status_and_diff_like_the_fake_engine() {
    let (real, fake) = (Real::new(), Fake::new());
    let (rw, fw) = (
        real.checkout("conflict").await,
        fake.checkout("conflict").await,
    );
    write(&rw, "src/main.rs", b"fn main() { b(); }\n");
    fake.write(&fw, "src/main.rs", b"fn main() { b(); }\n");
    for repo in [&real.f.repo, &fake.repo] {
        repo.seed(
            "alice",
            "from a",
            &[put("src/main.rs", "fn main() { a(); }\n")],
        )
        .unwrap();
    }
    let scan = real.engine().status_remote(&rw, &ctx()).await.unwrap();
    let main = find(&scan.statuses, "src/main.rs");
    assert_eq!(main.text, NodeStatus::Modified);
    assert!(main.remote.as_ref().unwrap().out_of_date);

    real.update(&rw).await;
    fake.engine
        .update(
            &fw,
            &UpdateRequest {
                wc_id: fw.id.clone(),
                paths: None,
                rev: None,
                depth: None,
            },
            &ctx(),
        )
        .await
        .unwrap();
    let real_main = find(&real.status(&rw).await, "src/main.rs").clone();
    let fake_main = find(&fake.status(&fw).await, "src/main.rs").clone();
    assert_eq!(real_main.text, NodeStatus::Conflicted);
    assert_eq!(real_main.text, fake_main.text);

    let diff = real.diff(&rw, "src/main.rs", None).await;
    assert_eq!(diff.left.unwrap().content, "fn main() { a(); }\n");
    let right = diff.right.unwrap().content;
    assert!(right.contains("<<<<<<<") && right.contains("b();") && right.contains("a();"));

    // The conflict marker files are listed as unversioned and can be previewed as additions.
    let st = real.status(&rw).await;
    let markers: Vec<&FileStatus> = st
        .iter()
        .filter(|s| s.path.starts_with("src/main.rs.") && s.text == NodeStatus::Unversioned)
        .collect();
    assert!(markers.len() >= 2, "{st:?}");
    for m in markers {
        let d = real.diff(&rw, &m.path, None).await;
        assert!(d.left.is_none(), "{}", m.path);
        let content = d
            .right
            .unwrap_or_else(|| panic!("{} has a text side", m.path))
            .content;
        assert!(content.contains("main"), "{}: {content}", m.path);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_reads_stop_before_touching_the_working_copy() {
    let real = Real::new();
    let rw = real.checkout("cancel").await;
    let cancelled = ctx();
    cancelled.cancel.cancel();
    let err = real
        .engine()
        .status(&rw, ALL, &cancelled)
        .await
        .expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
    let err = real
        .engine()
        .diff_wc(&rw, "README.md", None, DiffOptions::default(), &cancelled)
        .await
        .expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
    let err = real
        .engine()
        .status_remote(&rw, &cancelled)
        .await
        .expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn status_of_a_folder_that_is_not_a_working_copy_fails_cleanly() {
    let real = Real::new();
    let wc = WcRef {
        id: WcId("none".into()),
        root: real.dir.path().to_path_buf(),
    };
    let err = real.engine().status(&wc, ALL, &ctx()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotAWorkingCopy);
}

/// Connector that records the options of every `connect_with` call.
struct RecordingConnector {
    inner: StaticConnector,
    seen: std::sync::Mutex<Vec<crate::engine::real::ConnectOptions>>,
}

#[async_trait::async_trait]
impl crate::engine::real::SessionConnector for RecordingConnector {
    async fn connect(
        &self,
        url: &svn_types::RepoUrl,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, svn_types::SvnError> {
        self.inner.connect(url, cancel).await
    }

    async fn connect_with(
        &self,
        url: &svn_types::RepoUrl,
        options: &crate::engine::real::ConnectOptions,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, svn_types::SvnError> {
        self.seen.lock().unwrap().push(*options);
        self.inner.connect(url, cancel).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_status_asks_for_a_session_at_the_working_copy_url_with_the_call_interactivity() {
    use crate::engine::real::{Anchor, ConnectOptions};
    let repo = flat_repo();
    let connector = Arc::new(RecordingConnector {
        inner: StaticConnector(vec![repo.clone()]),
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let real = Real {
        f: fixture_on(repo, connector.clone(), Arc::new(ClientRecovery)),
        dir: tempfile::tempdir().unwrap(),
    };
    let rw = real.checkout("anchored").await;
    real.engine().status_remote(&rw, &ctx()).await.unwrap();
    real.engine()
        .status_remote(&rw, &OpContext::background())
        .await
        .unwrap();
    let seen = connector.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            ConnectOptions {
                anchor: Anchor::Url,
                interactive: true
            },
            ConnectOptions {
                anchor: Anchor::Url,
                interactive: false
            },
        ]
    );
}

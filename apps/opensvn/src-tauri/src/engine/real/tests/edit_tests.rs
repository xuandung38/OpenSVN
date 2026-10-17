//! Server-side edits: one revision per plan, copy semantics and conflicting plans.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opensvn_ipc::{CopyLocationRequest, Rev};
use svn_ra_api::{CommitEditor, CommitInfo, ReplayableSource};
use svn_types::{
    Checksum, DirId, ErrorCode, FileId, NodeKind, PropChange, RelPath, Result as SvnResult, Revnum,
};

use super::super::remote_edit::{Edit, Tree, drive};
use super::support::{StaticConnector, URL, ctx, fixture, fixture_on, standard_repo};
use crate::engine::real::ClientRecovery;
use crate::engine::{EditOp, EngineFacade, RemoteEditPlan};

#[tokio::test]
async fn one_plan_becomes_one_revision() {
    let f = fixture();
    let upload_dir = tempfile::tempdir().unwrap();
    let local = upload_dir.path().join("notes.txt");
    std::fs::write(&local, "uploaded\n").unwrap();
    let before = f.engine.repo_info(URL, &ctx()).await.unwrap().youngest_rev;

    let rev = f
        .engine
        .remote_edit(
            &RemoteEditPlan {
                ops: vec![
                    EditOp::Mkdir {
                        url: format!("{URL}/trunk/docs"),
                    },
                    EditOp::Upload {
                        url: format!("{URL}/trunk/docs/notes.txt"),
                        local_path: local.clone(),
                    },
                    EditOp::Upload {
                        url: format!("{URL}/trunk/README.md"),
                        local_path: local,
                    },
                    EditOp::Delete {
                        url: format!("{URL}/trunk/src"),
                    },
                ],
                message: "Reorganize".into(),
            },
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(rev.0, before.0 + 1, "all operations share one revision");

    let notes = f
        .engine
        .repo_cat(&format!("{URL}/trunk/docs/notes.txt"), None, &ctx())
        .await
        .unwrap();
    assert_eq!(notes.content, "uploaded\n");
    let readme = f
        .engine
        .repo_cat(&format!("{URL}/trunk/README.md"), None, &ctx())
        .await
        .unwrap();
    assert_eq!(readme.content, "uploaded\n", "an existing file is replaced");
    let gone = f
        .engine
        .repo_list(&format!("{URL}/trunk/src"), None, &ctx())
        .await
        .unwrap_err();
    assert_eq!(gone.code, ErrorCode::PathNotFound);
}

#[tokio::test]
async fn failed_plans_leave_no_revision_and_no_transaction() {
    let f = fixture();
    let before = f.engine.repo_info(URL, &ctx()).await.unwrap().youngest_rev;
    let err = f
        .engine
        .remote_edit(
            &RemoteEditPlan {
                ops: vec![EditOp::Mkdir {
                    url: format!("{URL}/trunk/src"),
                }],
                message: "Already there".into(),
            },
            &ctx(),
        )
        .await
        .unwrap_err();
    // The fake answers an existing path like svn: out of date (E160020, already exists).
    assert_eq!(err.code, ErrorCode::OutOfDate);
    assert_eq!(
        f.engine.repo_info(URL, &ctx()).await.unwrap().youngest_rev,
        before
    );
    assert!(f.repo.open_txns().is_empty(), "the transaction was aborted");
}

#[tokio::test]
async fn plans_are_validated_before_anything_is_sent() {
    let f = fixture();
    let empty = f
        .engine
        .remote_edit(
            &RemoteEditPlan {
                ops: Vec::new(),
                message: "nothing".into(),
            },
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_eq!(empty.code, ErrorCode::Internal);

    let relative = f
        .engine
        .remote_edit(
            &RemoteEditPlan {
                ops: vec![EditOp::Upload {
                    url: format!("{URL}/trunk/x.txt"),
                    local_path: "relative.txt".into(),
                }],
                message: "bad".into(),
            },
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(relative.message.contains("absolute"));

    let conflicting = f
        .engine
        .remote_edit(
            &RemoteEditPlan {
                ops: vec![
                    EditOp::Delete {
                        url: format!("{URL}/trunk/src"),
                    },
                    EditOp::Mkdir {
                        url: format!("{URL}/trunk/src/inner"),
                    },
                ],
                message: "bad".into(),
            },
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(conflicting.message.contains("conflicting"));
    assert_eq!(
        f.engine.repo_info(URL, &ctx()).await.unwrap().youngest_rev,
        Rev(1)
    );
}

#[tokio::test]
async fn copy_reports_what_the_repository_cannot_do() {
    // A repository without server-side copies: the engine passes the copy source to the commit
    // editor and surfaces the editor's refusal unchanged, leaving no transaction behind.
    let repo = standard_repo().without_copies();
    let connector = Arc::new(StaticConnector(vec![repo.clone()]));
    let f = fixture_on(repo, connector, Arc::new(ClientRecovery));
    let err = f
        .engine
        .copy_location(&branch_request("branches/b1"), None, &ctx())
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ServerUnsupported);
    assert!(f.repo.open_txns().is_empty());

    let exists = f
        .engine
        .copy_location(&branch_request("trunk/src"), None, &ctx())
        .await
        .unwrap_err();
    assert!(exists.message.contains("already exists"));
}

#[tokio::test]
async fn copy_creates_the_branch_from_its_source_url() {
    let f = fixture();
    let before = f.engine.repo_info(URL, &ctx()).await.unwrap().youngest_rev;
    let rev = f
        .engine
        .copy_location(&branch_request("branches/b1"), None, &ctx())
        .await
        .unwrap();
    assert_eq!(rev.0, before.0 + 1);
    let readme = f
        .engine
        .repo_cat(&format!("{URL}/branches/b1/README.md"), None, &ctx())
        .await
        .unwrap();
    assert_eq!(readme.content, "hello\n");
    assert!(f.repo.open_txns().is_empty());
}

fn branch_request(to: &str) -> CopyLocationRequest {
    CopyLocationRequest {
        from_url: format!("{URL}/trunk"),
        from_rev: None,
        to_url: format!("{URL}/{to}"),
        message: "Branch".into(),
        switch_wc_id: None,
        from_wc: None,
    }
}

/// Editor that records the calls it receives.
#[derive(Default, Clone)]
struct Recorder(Arc<Mutex<Vec<String>>>);

impl Recorder {
    fn log(&self, line: String) {
        self.0.lock().unwrap().push(line);
    }
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

#[async_trait]
impl CommitEditor for Recorder {
    async fn open_root(&mut self, _base: Option<Revnum>) -> SvnResult<DirId> {
        Ok(DirId(0))
    }
    async fn add_directory(
        &mut self,
        path: &RelPath,
        _parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> SvnResult<DirId> {
        self.log(format!("add_dir {path} from {copy_from:?}"));
        Ok(DirId(1))
    }
    async fn open_directory(
        &mut self,
        path: &RelPath,
        _parent: DirId,
        base: Option<Revnum>,
    ) -> SvnResult<DirId> {
        self.log(format!("open_dir {path} base {base:?}"));
        Ok(DirId(2))
    }
    async fn add_file(
        &mut self,
        path: &RelPath,
        _parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> SvnResult<FileId> {
        self.log(format!("add_file {path} from {copy_from:?}"));
        Ok(FileId(1))
    }
    async fn open_file(
        &mut self,
        path: &RelPath,
        _parent: DirId,
        _base: Option<Revnum>,
    ) -> SvnResult<FileId> {
        self.log(format!("open_file {path}"));
        Ok(FileId(2))
    }
    async fn apply_text(
        &mut self,
        _file: FileId,
        _base: Option<ReplayableSource>,
        target: ReplayableSource,
        _base_md5: Option<Checksum>,
        _result_md5: Checksum,
    ) -> SvnResult<()> {
        self.log(format!("text {}", target.size.unwrap_or(0)));
        Ok(())
    }
    async fn change_dir_prop(&mut self, _dir: DirId, _change: PropChange) -> SvnResult<()> {
        Ok(())
    }
    async fn change_file_prop(&mut self, _file: FileId, _change: PropChange) -> SvnResult<()> {
        Ok(())
    }
    async fn delete_entry(
        &mut self,
        path: &RelPath,
        _rev: Option<Revnum>,
        _parent: DirId,
    ) -> SvnResult<()> {
        self.log(format!("delete {path}"));
        Ok(())
    }
    async fn close_file(&mut self, _file: FileId) -> SvnResult<()> {
        Ok(())
    }
    async fn close_directory(&mut self, _dir: DirId) -> SvnResult<()> {
        Ok(())
    }
    async fn close_edit(self: Box<Self>) -> SvnResult<CommitInfo> {
        Err(svn_types::SvnError::new(
            ErrorCode::Internal,
            "not driven to the end in these tests",
        ))
    }
    async fn abort_edit(self: Box<Self>) -> SvnResult<svn_ra_api::TxnAbort> {
        Ok(svn_ra_api::TxnAbort::Deleted)
    }
}

fn path(p: &str) -> RelPath {
    RelPath::new(p).unwrap()
}

#[tokio::test]
async fn copies_are_added_with_their_source_and_renames_delete_the_origin() {
    let mut tree = Tree::default();
    tree.insert(
        &path("branches/b1"),
        Edit::Copy {
            from: format!("{URL}/trunk"),
            rev: Revnum(7),
            kind: NodeKind::Dir,
        },
    )
    .unwrap();
    tree.insert(
        &path("tags/README.md"),
        Edit::Copy {
            from: format!("{URL}/trunk/README.md"),
            rev: Revnum(7),
            kind: NodeKind::File,
        },
    )
    .unwrap();
    tree.insert(&path("old"), Edit::Delete).unwrap();

    let mut rec = Recorder::default();
    let seen = rec.clone();
    drive(&mut rec, &tree, &RelPath::root(), DirId(0), Revnum(7))
        .await
        .unwrap();
    assert_eq!(
        seen.lines(),
        vec![
            "open_dir branches base Some(Revnum(7))",
            &format!("add_dir branches/b1 from Some((\"{URL}/trunk\", Revnum(7)))"),
            "delete old",
            "open_dir tags base Some(Revnum(7))",
            &format!("add_file tags/README.md from Some((\"{URL}/trunk/README.md\", Revnum(7)))"),
        ]
    );
}

#[test]
fn a_path_carries_one_change_and_removed_entries_have_no_children() {
    let mut tree = Tree::default();
    tree.insert(&path("a/b"), Edit::Delete).unwrap();
    assert!(tree.insert(&path("a/b"), Edit::Mkdir).is_err());
    assert!(tree.insert(&path("a/b/c"), Edit::Mkdir).is_err());
    // Creating a directory and then a file below it is one coherent plan.
    tree.insert(&path("n"), Edit::Mkdir).unwrap();
    tree.insert(&path("n/f"), Edit::Mkdir).unwrap();
    // An entry that already has children below it cannot become a delete.
    assert!(tree.insert(&path("n"), Edit::Delete).is_err());
}

#[test]
fn the_repository_root_is_never_an_edit_target() {
    let mut tree = Tree::default();
    for edit in [Edit::Mkdir, Edit::Delete] {
        let err = tree.insert(&RelPath::root(), edit).unwrap_err();
        assert!(err.message.contains("repository root"), "{err}");
    }
}

#[tokio::test]
async fn deleting_the_repository_root_creates_no_revision() {
    let f = fixture();
    let before = f.engine.repo_info(URL, &ctx()).await.unwrap().youngest_rev;
    let err = f
        .engine
        .remote_edit(
            &RemoteEditPlan {
                ops: vec![EditOp::Delete {
                    url: URL.to_owned(),
                }],
                message: "Delete everything".into(),
            },
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(err.message.contains("repository root"), "{err}");
    let after = f.engine.repo_info(URL, &ctx()).await.unwrap().youngest_rev;
    assert_eq!(after, before, "no empty revision was committed");
    assert!(f.repo.open_txns().is_empty());
}

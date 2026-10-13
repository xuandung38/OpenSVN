//! Server-side edits (`remote_edit`, `copy_location`): the planned operations are merged into a
//! tree of changes and driven through one commit editor, so the whole plan is one revision.

use std::collections::BTreeMap;
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use md5::{Digest, Md5};
use opensvn_ipc::{CopyLocationRequest, Rev};
use svn_ra_api::{CommitEditor, CommitInfo, CommitObserver, RaSession, ReplayableSource};
use svn_types::{Checksum, DirId, ErrorCode, NodeKind, RelPath, Revnum, SvnError};

use super::RealEngine;
use super::mapping::{self, revnum};
use super::remote::internal;
use super::wc_read::blocking;
use crate::engine::context::OpContext;
use crate::engine::facade::{EditOp, EngineResult, RemoteEditPlan};

/// Largest local file `remote_edit` uploads.
const UPLOAD_LIMIT: u64 = 64 * 1024 * 1024;

/// One change at a path.
#[derive(Debug)]
pub(super) enum Edit {
    /// New directory.
    Mkdir,
    /// Remove the entry.
    Delete,
    /// New entry copied from `(repository path, revision)`.
    Copy {
        from: String,
        rev: Revnum,
        kind: NodeKind,
    },
    /// New or replaced file with content from a local file.
    Upload {
        local: PathBuf,
        md5: [u8; 16],
        size: u64,
        exists: bool,
    },
}

/// Changes grouped by directory, in the order the editor must see them.
#[derive(Debug, Default)]
pub(super) struct Tree {
    edit: Option<Edit>,
    children: BTreeMap<String, Tree>,
}

impl Tree {
    /// Records `edit` at `path`; one path carries one change, and entries that are removed,
    /// copied or uploaded have no children of their own in the same commit.
    pub(super) fn insert(&mut self, path: &RelPath, edit: Edit) -> EngineResult<()> {
        let mut node = self;
        for name in path.components() {
            if matches!(
                node.edit,
                Some(Edit::Delete | Edit::Copy { .. } | Edit::Upload { .. })
            ) {
                return Err(internal(format!("conflicting edits below /{path}")));
            }
            node = node.children.entry(name.to_owned()).or_default();
        }
        if node.edit.is_some() || (!node.children.is_empty() && !matches!(edit, Edit::Mkdir)) {
            return Err(internal(format!("conflicting edits for /{path}")));
        }
        node.edit = Some(edit);
        Ok(())
    }
}

/// The commit is not journaled: a remote edit has no working copy to finalize.
struct NoJournal;

impl CommitObserver for NoJournal {
    fn on_txn_created(&self, _txn: &str) -> svn_types::Result<()> {
        Ok(())
    }

    fn on_merged(&self, _info: &CommitInfo) -> svn_types::Result<()> {
        Ok(())
    }
}

type DriveFuture<'a> = Pin<Box<dyn Future<Output = EngineResult<()>> + Send + 'a>>;

/// Feeds the children of `dir` (at `path`) to the editor.
pub(super) fn drive<'a>(
    editor: &'a mut dyn CommitEditor,
    node: &'a Tree,
    path: &'a RelPath,
    dir: DirId,
    head: Revnum,
) -> DriveFuture<'a> {
    Box::pin(async move {
        for (name, child) in &node.children {
            let child_path = path.child(name)?;
            match &child.edit {
                None => {
                    let d = editor.open_directory(&child_path, dir, Some(head)).await?;
                    drive(editor, child, &child_path, d, head).await?;
                    editor.close_directory(d).await?;
                }
                Some(Edit::Mkdir) => {
                    let d = editor.add_directory(&child_path, dir, None).await?;
                    drive(editor, child, &child_path, d, head).await?;
                    editor.close_directory(d).await?;
                }
                Some(Edit::Delete) => {
                    editor.delete_entry(&child_path, Some(head), dir).await?;
                }
                Some(Edit::Copy { from, rev, kind }) => {
                    let source = Some((from.as_str(), *rev));
                    if *kind == NodeKind::Dir {
                        let d = editor.add_directory(&child_path, dir, source).await?;
                        editor.close_directory(d).await?;
                    } else {
                        let f = editor.add_file(&child_path, dir, source).await?;
                        editor.close_file(f).await?;
                    }
                }
                Some(Edit::Upload {
                    local,
                    md5,
                    size,
                    exists,
                }) => {
                    let file = if *exists {
                        editor.open_file(&child_path, dir, Some(head)).await?
                    } else {
                        editor.add_file(&child_path, dir, None).await?
                    };
                    editor
                        .apply_text(
                            file,
                            None,
                            ReplayableSource {
                                path: local.clone(),
                                translated: true,
                                size: Some(*size),
                            },
                            None,
                            Checksum::Md5(*md5),
                        )
                        .await?;
                    editor.close_file(file).await?;
                }
            }
        }
        Ok(())
    })
}

/// MD5 of a local file plus its size, read in a blocking task.
async fn digest_local_file(path: &Path) -> EngineResult<([u8; 16], u64)> {
    if !path.is_absolute() {
        return Err(internal("upload path must be absolute"));
    }
    let path = path.to_path_buf();
    blocking(move || {
        let shown = path.display();
        let io = |e: std::io::Error| SvnError::from_io(format!("read {shown}"), &e);
        let mut file = std::fs::File::open(&path).map_err(io)?;
        let size = file.metadata().map_err(io)?.len();
        if size > UPLOAD_LIMIT {
            return Err(internal("file is too large to upload in this build"));
        }
        let mut hasher = Md5::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).map_err(io)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok((hasher.finalize().into(), size))
    })
    .await
}

/// Absolute repository path for a copy source.
fn fspath(p: &RelPath) -> String {
    format!("/{p}")
}

impl RealEngine {
    /// Commits `tree` as one revision through a commit editor.
    async fn commit_tree(
        &self,
        session: &dyn RaSession,
        message: &str,
        tree: &Tree,
        ctx: &OpContext,
    ) -> EngineResult<Rev> {
        let head = session.youngest_rev(&ctx.cancel).await?;
        let mut editor = session
            .commit_editor(message, &[], &[], false, Arc::new(NoJournal), &ctx.cancel)
            .await?;
        let driven = async {
            let root = editor.open_root(Some(head)).await?;
            drive(editor.as_mut(), tree, &RelPath::root(), root, head).await?;
            editor.close_directory(root).await?;
            EngineResult::Ok(())
        }
        .await;
        match driven {
            Ok(()) => Ok(mapping::rev(editor.close_edit().await?.revision)),
            Err(e) => {
                // `abort_edit` works after cancellation too; the original error is what matters.
                if let Err(abort) = editor.abort_edit().await {
                    tracing::warn!(error = %abort, "could not abort the commit transaction");
                }
                Err(e)
            }
        }
    }

    /// Adds a copy of `from` (at `rev`, default head) as `to` to `tree`.
    async fn plan_copy(
        &self,
        session: &dyn RaSession,
        tree: &mut Tree,
        (from, rev): (&RelPath, Option<Revnum>),
        to: &RelPath,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        let rev = match rev {
            Some(r) => r,
            None => session.youngest_rev(&ctx.cancel).await?,
        };
        let kind = session.check_path(from, Some(rev), &ctx.cancel).await?;
        if !matches!(kind, NodeKind::File | NodeKind::Dir) {
            return Err(SvnError::new(
                ErrorCode::PathNotFound,
                format!("/{from} does not exist at r{}", rev.0),
            ));
        }
        tree.insert(
            to,
            Edit::Copy {
                from: fspath(from),
                rev,
                kind,
            },
        )
    }

    /// Locates `url`; every URL of one plan must belong to the repository of the first one.
    async fn locate_in_plan(
        &self,
        url: &str,
        first: &Option<Arc<dyn RaSession>>,
        ctx: &OpContext,
    ) -> EngineResult<(Arc<dyn RaSession>, RelPath)> {
        let (session, path) = self.locate(url, ctx).await?;
        match first {
            Some(prev) if prev.uuid() != session.uuid() => {
                Err(internal("remote edit spans several repositories"))
            }
            _ => Ok((session, path)),
        }
    }

    pub(super) async fn do_remote_edit(
        &self,
        plan: &RemoteEditPlan,
        ctx: &OpContext,
    ) -> EngineResult<Rev> {
        let mut session: Option<Arc<dyn RaSession>> = None;
        let mut tree = Tree::default();
        for op in &plan.ops {
            match op {
                EditOp::Mkdir { url } => {
                    let (s, p) = self.locate_in_plan(url, &session, ctx).await?;
                    session = Some(s);
                    tree.insert(&p, Edit::Mkdir)?;
                }
                EditOp::Delete { url } => {
                    let (s, p) = self.locate_in_plan(url, &session, ctx).await?;
                    session = Some(s);
                    tree.insert(&p, Edit::Delete)?;
                }
                EditOp::Upload { url, local_path } => {
                    let (s, p) = self.locate_in_plan(url, &session, ctx).await?;
                    let (md5, size) = digest_local_file(local_path).await?;
                    let exists = s.check_path(&p, None, &ctx.cancel).await? == NodeKind::File;
                    session = Some(s);
                    tree.insert(
                        &p,
                        Edit::Upload {
                            local: local_path.clone(),
                            md5,
                            size,
                            exists,
                        },
                    )?;
                }
                EditOp::Copy {
                    from_url,
                    from_rev,
                    to_url,
                } => {
                    let (s, from) = self.locate_in_plan(from_url, &session, ctx).await?;
                    session = Some(s.clone());
                    let (_, to) = self.locate_in_plan(to_url, &session, ctx).await?;
                    self.plan_copy(s.as_ref(), &mut tree, (&from, revnum(*from_rev)), &to, ctx)
                        .await?;
                }
                EditOp::Rename { from_url, to_url } => {
                    let (s, from) = self.locate_in_plan(from_url, &session, ctx).await?;
                    session = Some(s.clone());
                    let (_, to) = self.locate_in_plan(to_url, &session, ctx).await?;
                    self.plan_copy(s.as_ref(), &mut tree, (&from, None), &to, ctx)
                        .await?;
                    tree.insert(&from, Edit::Delete)?;
                }
            }
        }
        let session = session.ok_or_else(|| internal("remote edit has no operations"))?;
        self.commit_tree(session.as_ref(), &plan.message, &tree, ctx)
            .await
    }

    pub(super) async fn do_copy_location(
        &self,
        req: &CopyLocationRequest,
        ctx: &OpContext,
    ) -> EngineResult<Rev> {
        let (session, from) = self.locate(&req.from_url, ctx).await?;
        let (to_session, to) = self.locate(&req.to_url, ctx).await?;
        if session.uuid() != to_session.uuid() {
            return Err(internal("cannot copy between repositories"));
        }
        if session.check_path(&to, None, &ctx.cancel).await? != NodeKind::None {
            return Err(internal(format!("{} already exists", req.to_url)));
        }
        let mut tree = Tree::default();
        self.plan_copy(
            session.as_ref(),
            &mut tree,
            (&from, revnum(req.from_rev)),
            &to,
            ctx,
        )
        .await?;
        self.commit_tree(session.as_ref(), &req.message, &tree, ctx)
            .await
    }
}

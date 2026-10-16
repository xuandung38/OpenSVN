//! Repository-side edits without a working copy: new folder, delete, copy (branch/tag) and
//! move/rename, each a single commit.
//!
//! The functions work on any [`RaSession`]. All paths are relative to the session URL, so open
//! the session at a directory that contains every path involved (the repository root for a
//! branch or tag copy). Several operations passed together become one commit.

use std::sync::Arc;

use svn_ra_api::{CommitEditor, CommitInfo, CommitObserver, RaSession};
use svn_types::{
    CancelToken, DirId, ErrorCode, NodeKind, PropChange, RelPath, Result, Revnum, SvnError,
};

/// One change of a remote commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteOp {
    /// Create a directory; its parent must exist.
    Mkdir(RelPath),
    /// Delete a node. `rev` is the revision the caller saw (`None` = whatever is newest).
    Delete {
        /// Node to delete.
        path: RelPath,
        /// Revision the caller based the decision on.
        rev: Option<Revnum>,
    },
    /// Copy `src` at `src_rev` (`None` = HEAD) to the new path `dst`.
    Copy {
        /// Source path.
        src: RelPath,
        /// Source revision.
        src_rev: Option<Revnum>,
        /// Destination path (must not exist).
        dst: RelPath,
    },
    /// Copy `src` to `dst` and delete `src` in the same commit.
    Move {
        /// Source path.
        src: RelPath,
        /// Source revision (`None` = HEAD).
        src_rev: Option<Revnum>,
        /// Destination path (must not exist).
        dst: RelPath,
    },
}

/// Log message, revision properties and journal hook of a remote commit.
#[derive(Clone)]
pub struct RemoteCommit {
    /// Log message.
    pub message: String,
    /// Extra revision properties.
    pub rev_props: Vec<PropChange>,
    /// Told about the transaction and the new revision.
    pub observer: Arc<dyn CommitObserver>,
}

impl std::fmt::Debug for RemoteCommit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteCommit")
            .field("message", &self.message)
            .finish_non_exhaustive()
    }
}

/// What the commit does at one path, after sources were resolved.
enum Edit {
    AddDir,
    Delete(Option<Revnum>),
    Copy {
        from: String,
        rev: Revnum,
        kind: NodeKind,
    },
}

fn invalid(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::Internal, msg)
}

fn not_root(path: &RelPath) -> Result<()> {
    if path.is_root() {
        Err(invalid("cannot change the session root itself"))
    } else {
        Ok(())
    }
}

async fn resolve_copy(
    session: &dyn RaSession,
    src: &RelPath,
    src_rev: Option<Revnum>,
    cancel: &CancelToken,
) -> Result<(String, Revnum, NodeKind)> {
    not_root(src)?;
    let rev = match src_rev {
        Some(r) => r,
        None => session.youngest_rev(cancel).await?,
    };
    let kind = session.check_path(src, Some(rev), cancel).await?;
    if !matches!(kind, NodeKind::File | NodeKind::Dir) {
        return Err(SvnError::new(
            ErrorCode::PathNotFound,
            format!("copy source {src} does not exist at revision {rev}"),
        ));
    }
    let url = session.session_url().join(src)?;
    Ok((url.to_string(), rev, kind))
}

/// Turns the requested operations into edits keyed by path.
async fn plan(
    session: &dyn RaSession,
    ops: &[RemoteOp],
    cancel: &CancelToken,
) -> Result<Vec<(RelPath, Edit)>> {
    let mut edits = Vec::new();
    for op in ops {
        match op {
            RemoteOp::Mkdir(path) => {
                not_root(path)?;
                edits.push((path.clone(), Edit::AddDir));
            }
            RemoteOp::Delete { path, rev } => {
                not_root(path)?;
                edits.push((path.clone(), Edit::Delete(*rev)));
            }
            RemoteOp::Copy { src, src_rev, dst } => {
                not_root(dst)?;
                let (from, rev, kind) = resolve_copy(session, src, *src_rev, cancel).await?;
                edits.push((dst.clone(), Edit::Copy { from, rev, kind }));
            }
            RemoteOp::Move { src, src_rev, dst } => {
                not_root(dst)?;
                if dst.starts_with(src) {
                    return Err(invalid(format!("cannot move {src} into itself")));
                }
                let (from, rev, kind) = resolve_copy(session, src, *src_rev, cancel).await?;
                edits.push((dst.clone(), Edit::Copy { from, rev, kind }));
                edits.push((src.clone(), Edit::Delete(Some(rev))));
            }
        }
    }
    // Depth-first editor order: compare paths component by component, so every directory's
    // children come right after it (`a`, `a/c`, `a-b`; a plain string sort would put `a-b`
    // between `a` and `a/c` and reopen `a`). Deletes before adds at the same path; stable
    // otherwise.
    edits.sort_by(|a, b| {
        a.0.as_str()
            .split('/')
            .cmp(b.0.as_str().split('/'))
            .then_with(|| matches!(b.1, Edit::Delete(_)).cmp(&matches!(a.1, Edit::Delete(_))))
    });
    Ok(edits)
}

/// Directories opened on the way to the current edit, root first.
struct OpenDirs(Vec<(RelPath, DirId)>);

impl OpenDirs {
    /// Closes and opens directories until the innermost one is `target`.
    async fn goto(&mut self, editor: &mut dyn CommitEditor, target: &RelPath) -> Result<DirId> {
        while self
            .0
            .last()
            .is_some_and(|(p, _)| !p.is_root() && !target.starts_with(p))
        {
            if let Some((_, id)) = self.0.pop() {
                editor.close_directory(id).await?;
            }
        }
        loop {
            let Some((current, id)) = self.0.last().cloned() else {
                return Err(invalid("no open directory"));
            };
            if &current == target {
                return Ok(id);
            }
            let rest = target
                .as_str()
                .strip_prefix(current.as_str())
                .map_or("", |r| r.trim_start_matches('/'));
            let next = rest.split('/').next().unwrap_or_default();
            let child = current.child(next)?;
            let child_id = editor.open_directory(&child, id, None).await?;
            self.0.push((child, child_id));
        }
    }

    async fn close_all(&mut self, editor: &mut dyn CommitEditor) -> Result<()> {
        while let Some((_, id)) = self.0.pop() {
            editor.close_directory(id).await?;
        }
        Ok(())
    }
}

async fn apply(
    editor: &mut dyn CommitEditor,
    edits: &[(RelPath, Edit)],
    cancel: &CancelToken,
) -> Result<()> {
    let root = editor.open_root(None).await?;
    let mut open = OpenDirs(vec![(RelPath::root(), root)]);
    for (path, edit) in edits {
        cancel.check()?;
        let parent_path = path.parent().unwrap_or_else(RelPath::root);
        let parent = open.goto(editor, &parent_path).await?;
        match edit {
            Edit::AddDir => {
                let id = editor.add_directory(path, parent, None).await?;
                editor.close_directory(id).await?;
            }
            Edit::Delete(rev) => editor.delete_entry(path, *rev, parent).await?,
            Edit::Copy { from, rev, kind } => {
                let copy = Some((from.as_str(), *rev));
                if *kind == NodeKind::Dir {
                    let id = editor.add_directory(path, parent, copy).await?;
                    editor.close_directory(id).await?;
                } else {
                    let id = editor.add_file(path, parent, copy).await?;
                    editor.close_file(id).await?;
                }
            }
        }
    }
    open.close_all(editor).await
}

/// Runs `ops` as one commit and returns the new revision.
pub async fn commit_ops(
    session: &dyn RaSession,
    ops: &[RemoteOp],
    commit: &RemoteCommit,
    cancel: &CancelToken,
) -> Result<CommitInfo> {
    if ops.is_empty() {
        return Err(invalid("nothing to commit"));
    }
    let edits = plan(session, ops, cancel).await?;
    let mut editor = session
        .commit_editor(
            &commit.message,
            &commit.rev_props,
            &[],
            false,
            Arc::clone(&commit.observer),
            cancel,
        )
        .await?;
    match apply(editor.as_mut(), &edits, cancel).await {
        Ok(()) => editor.close_edit().await,
        Err(e) => {
            // The original failure is what the caller needs; the abort only cleans up, and
            // turns a broken connection into "nothing committed" once the transaction is gone.
            let abort = editor.abort_edit().await;
            Err(svn_ra_api::unsent_commit_error(e, &abort))
        }
    }
}

/// Creates directories (`svn mkdir URL`).
pub async fn mkdir(
    session: &dyn RaSession,
    paths: &[RelPath],
    commit: &RemoteCommit,
    cancel: &CancelToken,
) -> Result<CommitInfo> {
    let ops: Vec<_> = paths.iter().cloned().map(RemoteOp::Mkdir).collect();
    commit_ops(session, &ops, commit, cancel).await
}

/// Deletes nodes (`svn delete URL`).
pub async fn delete(
    session: &dyn RaSession,
    paths: &[RelPath],
    commit: &RemoteCommit,
    cancel: &CancelToken,
) -> Result<CommitInfo> {
    let ops: Vec<_> = paths
        .iter()
        .map(|path| RemoteOp::Delete {
            path: path.clone(),
            rev: None,
        })
        .collect();
    commit_ops(session, &ops, commit, cancel).await
}

/// Copies `src` at `src_rev` to `dst` (`svn copy URL URL`; used for branches and tags).
pub async fn copy(
    session: &dyn RaSession,
    src: &RelPath,
    src_rev: Option<Revnum>,
    dst: &RelPath,
    commit: &RemoteCommit,
    cancel: &CancelToken,
) -> Result<CommitInfo> {
    let op = RemoteOp::Copy {
        src: src.clone(),
        src_rev,
        dst: dst.clone(),
    };
    commit_ops(session, &[op], commit, cancel).await
}

/// Moves or renames `src` to `dst` (`svn move URL URL`).
pub async fn move_path(
    session: &dyn RaSession,
    src: &RelPath,
    dst: &RelPath,
    commit: &RemoteCommit,
    cancel: &CancelToken,
) -> Result<CommitInfo> {
    let op = RemoteOp::Move {
        src: src.clone(),
        src_rev: None,
        dst: dst.clone(),
    };
    commit_ops(session, &[op], commit, cancel).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use svn_ra_api::fake::FakeRaSession;

    #[tokio::test]
    async fn plan_orders_paths_depth_first() {
        let s = FakeRaSession::new("https://svn.test.local/repos/demo", "uuid-1").unwrap();
        let p = |s: &str| RelPath::new(s).unwrap();
        let ops = [
            RemoteOp::Mkdir(p("a-b")),
            RemoteOp::Mkdir(p("a/c")),
            RemoteOp::Mkdir(p("a")),
            RemoteOp::Mkdir(p("a.d/e")),
            RemoteOp::Delete {
                path: p("a"),
                rev: None,
            },
        ];
        let edits = plan(&s, &ops, &CancelToken::new()).await.unwrap();
        let order: Vec<(&str, bool)> = edits
            .iter()
            .map(|(path, e)| (path.as_str(), matches!(e, Edit::Delete(_))))
            .collect();
        assert_eq!(
            order,
            [
                ("a", true),
                ("a", false),
                ("a/c", false),
                ("a-b", false),
                ("a.d/e", false),
            ]
        );
    }
}

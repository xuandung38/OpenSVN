//! Executes an update plan against a delta editor through an [`EditorWorker`].

use std::collections::HashMap;
use std::sync::Mutex;

use svn_types::{
    CancelToken, DeltaEditor, DirId, ErrorCode, FileId, RelPath, Result, Revnum, SvnError,
    TxDeltaWindow,
};

use super::repo::{RepoState, Tree};
use super::update_plan::{CollectingReporter, Op, PlanInput, plan};
use crate::editor_worker::{DEFAULT_COMMAND_CAPACITY, EditorHandle, EditorWorker};
use crate::session::{ReportFn, UpdateMode, UpdateRequest};

fn lock_state(state: &Mutex<RepoState>) -> std::sync::MutexGuard<'_, RepoState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn url_to_rel(root: &svn_types::RepoUrl, url: &svn_types::RepoUrl) -> Result<RelPath> {
    let base = root.as_url().path().trim_end_matches('/');
    let full = url.as_url().path();
    let rest = full.strip_prefix(base).ok_or_else(|| {
        SvnError::new(ErrorCode::Internal, "switch URL is outside the repository")
    })?;
    RelPath::new(rest.trim_matches('/'))
}

/// Where the session of a drive is: repository root URL and the session URL relative to it.
pub(super) struct SessionAt<'a> {
    pub repos_root: &'a svn_types::RepoUrl,
    pub session_rel: &'a RelPath,
}

/// Runs a report-driven drive against the fake repository.
pub(super) async fn drive_update(
    state: &Mutex<RepoState>,
    at: SessionAt<'_>,
    max_open_files: usize,
    req: UpdateRequest,
    report: ReportFn,
    editor: Box<dyn DeltaEditor>,
    cancel: &CancelToken,
) -> Result<()> {
    cancel.check()?;
    let SessionAt {
        repos_root,
        session_rel,
    } = at;
    let mut reporter = CollectingReporter::default();
    report(&mut reporter)?;

    let tree_target = match &req.mode {
        UpdateMode::Switch(url) => url_to_rel(repos_root, url)?,
        _ => super::prefixed::join(session_rel, &req.target)?,
    };
    let (target_rev, target_tree, base_trees) = {
        let st = lock_state(state);
        let (rev, rv) = st.revision(req.revision)?;
        let mut bases: HashMap<Revnum, Tree> = HashMap::new();
        for e in reporter
            .entries
            .iter()
            .filter(|e| !e.deleted && !e.start_empty)
        {
            let (r, brv) = st.revision(Some(e.rev))?;
            // Report paths are session-relative: so are the base trees.
            bases.insert(r, super::prefixed::subtree(&brv.tree, session_rel)?);
        }
        (rev, rv.tree.clone(), bases)
    };
    let input = PlanInput {
        target_rev,
        target_tree: &target_tree,
        base_trees: &base_trees,
        entries: &reporter.entries,
        anchor_target: req.target.clone(),
        tree_target,
        send_text: req.mode != UpdateMode::Status,
        max_open_files,
        requested_depth: req.depth,
    };
    let ops = plan(&input)?;

    let handle = EditorWorker::spawn(editor, DEFAULT_COMMAND_CAPACITY);
    match execute(&handle, ops, cancel).await {
        Ok(()) => handle.close_edit().await,
        Err(e) => {
            // Surface the original failure; the abort result adds nothing.
            let _ = handle.abort_edit().await;
            Err(e)
        }
    }
}

async fn execute(h: &EditorHandle, ops: Vec<Op>, cancel: &CancelToken) -> Result<()> {
    let mut dirs: HashMap<usize, DirId> = HashMap::new();
    let mut files: HashMap<usize, FileId> = HashMap::new();
    let missing = || SvnError::new(ErrorCode::Internal, "plan references an unopened handle");
    for op in ops {
        cancel.check()?;
        match op {
            Op::SetTarget(rev) => h.set_target_revision(rev).await?,
            Op::OpenRoot(base) => {
                dirs.insert(0, h.open_root(base).await?);
            }
            Op::Dir {
                slot,
                parent,
                path,
                add,
                base,
            } => {
                let p = *dirs.get(&parent).ok_or_else(missing)?;
                let id = if add {
                    h.add_directory(&path, p, None).await?
                } else {
                    h.open_directory(&path, p, base).await?
                };
                dirs.insert(slot, id);
            }
            Op::DirProp(slot, change) => {
                h.change_dir_prop(*dirs.get(&slot).ok_or_else(missing)?, change)
                    .await?
            }
            Op::CloseDir(slot) => {
                h.close_directory(*dirs.get(&slot).ok_or_else(missing)?)
                    .await?
            }
            Op::Delete { path, rev, parent } => {
                h.delete_entry(&path, rev, *dirs.get(&parent).ok_or_else(missing)?)
                    .await?
            }
            Op::File {
                slot,
                parent,
                path,
                add,
                base,
            } => {
                let p = *dirs.get(&parent).ok_or_else(missing)?;
                let id = if add {
                    h.add_file(&path, p, None).await?
                } else {
                    h.open_file(&path, p, base).await?
                };
                files.insert(slot, id);
            }
            Op::FileProp(slot, change) => {
                h.change_file_prop(*files.get(&slot).ok_or_else(missing)?, change)
                    .await?
            }
            Op::Text(slot, content) => {
                let sender = h
                    .apply_textdelta(*files.get(&slot).ok_or_else(missing)?, None)
                    .await?;
                if !content.is_empty() {
                    sender.send(Some(TxDeltaWindow::full_text(content))).await?;
                }
                sender.send(None).await?;
            }
            Op::CloseFile(slot) => {
                h.close_file(*files.get(&slot).ok_or_else(missing)?, None)
                    .await?
            }
        }
    }
    Ok(())
}

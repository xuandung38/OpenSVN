//! The update editor: applies a server drive to a working copy.
//!
//! Safety model (see the module docs of [`super`]): every directory is marked `incomplete` before
//! it changes, every file or directory is committed to wc.db atomically together with the work
//! items that make the disk match (several nodes share one transaction, in order; see
//! [`super::batch`]), and cancellation or an aborted drive leaves `incomplete` markers so the next
//! update resumes.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use svn_types::{
    Checksum, DeltaEditor, Depth, DirId, ErrorCode, FileId, NodeKind, PropChange, RelPath, Result,
    Revnum, SvnError, WindowSink,
};
use svn_wc::conflicts::{TreeAction, TreeReason};
use svn_wc::db::{BaseNode, Node, Presence};
use svn_wc::pristine::Sha1Digest;
use svn_wc::workqueue::WorkItem;

use super::batch::{Barrier, Batch, Op, Staged};
use super::conflict_record::{self, NewConflict, Side};
use super::state::{
    CommittedNode, DirState, EntryProps, FileState, Outcome, OutcomeHandle, PendingTreeConflict,
    TextSlot, UpdateSetup, parse_svn_date, tmp_dir,
};
use super::text_sink::{BaseSource, DiscardSink, TextSink};
use crate::ctx::{UpdateAction, UpdateEvent};
use crate::wc;

/// Failpoint hook; compiled out without the `failpoints` feature.
#[cfg(feature = "failpoints")]
macro_rules! failpoint {
    ($name:literal) => {
        fail::fail_point!($name, |_| Err(svn_types::SvnError::new(
            svn_types::ErrorCode::Internal,
            concat!("failpoint ", $name)
        )));
    };
}

#[cfg(not(feature = "failpoints"))]
macro_rules! failpoint {
    ($name:literal) => {};
}
pub(super) use failpoint;

/// How long a cancelled drive waits for durability barriers before it gives up committing.
const CANCEL_GRACE: Duration = Duration::from_millis(500);

/// How often a barrier wait looks at the cancellation token.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Applies an update, switch or checkout drive to a working copy.
pub struct UpdateEditor {
    pub(super) s: UpdateSetup,
    pub(super) out: OutcomeHandle,
    pub(super) target_wc: RelPath,
    pub(super) target_rev: Option<Revnum>,
    next_id: u32,
    pub(super) dirs: HashMap<u32, DirState>,
    pub(super) files: HashMap<u32, FileState>,
    pub(super) skipped: Vec<RelPath>,
    /// Tree conflicts raised by `delete_entry` in this drive, with the BASE node that was
    /// deleted: an add of the same name that follows is a replacement (svn's
    /// `deletion_conflicts`), see [`UpdateEditor::take_replacement`].
    pub(super) deletion_conflicts: HashMap<RelPath, (Node, TreeReason)>,
    old_target_repos: Option<String>,
    finished: bool,
    /// Nodes prepared but not committed yet.
    pub(super) batch: Batch,
    /// The previous batch, waiting for its durability barrier.
    staged: Option<Staged>,
    /// Work items that ran after the last commit, completed with the next one.
    unsettled: svn_wc::workqueue::Unsettled,
    /// A batch could not be committed: nothing prepared after it may be committed either, or
    /// the committed state would no longer be a prefix of the drive (a file could land below a
    /// directory whose row was in the lost batch).
    poisoned: bool,
    /// Pristine texts placed by this drive whose rows are not committed yet; never replaced
    /// (see [`svn_wc::pristine::PristineStore::place`]).
    pub(super) placed: HashSet<Sha1Digest>,
    /// The working copy's filesystem honours one `syncfs` barrier per batch
    /// ([`svn_wc::durable::batched_sync`]); otherwise every file is synced on its own.
    pub(super) batched: bool,
    /// Set when the drive is cancelled: waiting for a barrier stops there, and what it would
    /// have covered is left uncommitted (the next update fetches it again).
    deadline: Option<Instant>,
    /// `.svn/tmp` exists.
    tmp_ready: bool,
}

fn corrupt(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::Corrupt, msg)
}

/// `parent/name` for repository paths (`""` is the root).
pub(super) fn repos_child(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

impl UpdateEditor {
    /// Creates the editor and the shared handle the caller reads results from.
    pub fn new(setup: UpdateSetup) -> Result<(Self, OutcomeHandle)> {
        let target_wc = wc::join_rel(&setup.anchor, &setup.target)?;
        let old_target_repos = if setup.switch_relpath.is_some() {
            setup
                .db
                .read_base(&target_wc)?
                .and_then(|n| n.repos_relpath)
        } else {
            None
        };
        let out: OutcomeHandle = Arc::new(Mutex::new(Outcome::default()));
        let batched = svn_wc::durable::batched_sync(setup.db.wcroot());
        Ok((
            Self {
                s: setup,
                out: Arc::clone(&out),
                target_wc,
                target_rev: None,
                next_id: 1,
                dirs: HashMap::new(),
                files: HashMap::new(),
                skipped: Vec::new(),
                deletion_conflicts: HashMap::new(),
                old_target_repos,
                finished: false,
                batch: Batch::default(),
                staged: None,
                unsettled: svn_wc::workqueue::Unsettled::default(),
                poisoned: false,
                placed: HashSet::new(),
                batched,
                deadline: None,
                tmp_ready: false,
            },
            out,
        ))
    }

    pub(super) fn is_finished(&self) -> bool {
        self.finished
    }

    pub(super) fn mark_finished(&mut self) {
        self.finished = true;
        self.out
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .completed = true;
    }

    pub(super) fn old_target(&self) -> Option<&str> {
        self.old_target_repos.as_deref()
    }

    pub(super) fn rev(&self) -> Result<u64> {
        self.target_rev
            .map(|r| r.0)
            .ok_or_else(|| SvnError::new(ErrorCode::Internal, "target revision was not announced"))
    }

    fn alloc(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Reports `action` on `path` (the node is prepared; its batch commits later, see
    /// [`super::batch`]).
    pub(super) fn emit(&mut self, path: &RelPath, kind: NodeKind, action: UpdateAction) {
        let ev = UpdateEvent {
            path: path.clone(),
            kind,
            action,
        };
        self.s.ctx.notify(&ev);
        self.out
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .events
            .push(ev);
    }

    /// Hands the database side of a prepared node at `path` to the batch; flushes when the
    /// batch is due. The node is finished once committed (see [`Self::defer_incomplete`]).
    pub(super) fn defer(&mut self, path: &RelPath, op: Op) -> Result<()> {
        self.defer_node(path, true, op)
    }

    /// [`Self::defer`] for a directory that is committed `incomplete`: its children follow.
    pub(super) fn defer_incomplete(&mut self, path: &RelPath, op: Op) -> Result<()> {
        self.defer_node(path, false, op)
    }

    fn defer_node(&mut self, path: &RelPath, complete: bool, op: Op) -> Result<()> {
        let node = CommittedNode {
            path: path.clone(),
            complete,
        };
        if self.batch.push(node, op) {
            self.flush()?;
        }
        Ok(())
    }

    /// Commits the previously staged batch and stages the pending nodes behind a durability
    /// barrier that runs on another thread (see [`super::batch`]).
    ///
    /// Cancellation is not checked here: what was prepared is committed (the queue run stops at
    /// the next item when cancelled and the rest stays queued).
    pub(super) fn flush(&mut self) -> Result<()> {
        if self.poisoned {
            // Dropped, not committed: the pristines stay as orphans and the directories keep
            // their `incomplete` markers, so the next update fetches these nodes again.
            drop(self.batch.take());
            return Err(SvnError::new(
                ErrorCode::Internal,
                "an earlier batch of the update could not be committed",
            ));
        }
        self.commit_staged()?;
        if self.batch.is_empty() && self.unsettled.is_empty() {
            return Ok(());
        }
        let pending = self.batch.take();
        let settle = std::mem::take(&mut self.unsettled);
        let ticket = (pending.unsynced || settle.unsynced)
            .then(|| Barrier::start(self.s.db.wcroot().to_path_buf(), settle.elsewhere.clone()));
        self.staged = Some(Staged {
            pending,
            settle,
            ticket,
        });
        Ok(())
    }

    /// Commits everything: the staged and the pending nodes, the work they queued and its
    /// completion. Afterwards wc.db and the disk agree and nothing is queued by this drive.
    pub(super) fn drain(&mut self) -> Result<()> {
        self.flush()?;
        self.commit_staged()?;
        let settle = std::mem::take(&mut self.unsettled);
        if !settle.is_empty() {
            if settle.unsynced {
                let root = self.s.db.wcroot();
                let barrier = Barrier::start(root.to_path_buf(), settle.elsewhere.clone());
                self.await_barrier(&barrier)?;
            }
            self.s.db.with_txn(|t| t.complete_work(&settle))?;
        }
        Ok(())
    }

    /// Waits for `barrier`. A cancellation, also one that arrives during the wait, bounds the
    /// wait to [`CANCEL_GRACE`]; past it nothing more is committed by this drive (the barrier
    /// finishes on its own thread, and nothing that relies on it commits).
    fn await_barrier(&mut self, barrier: &Barrier) -> Result<()> {
        #[cfg(feature = "failpoints")]
        fail::fail_point!("update.barrier_wait");
        loop {
            if self.deadline.is_none() && self.s.cancel.is_cancelled() {
                self.deadline = Some(Instant::now() + CANCEL_GRACE);
            }
            let step = Instant::now() + CANCEL_POLL;
            let until = self.deadline.map_or(step, |d| d.min(step));
            if let Some(r) = barrier.wait(Some(until)) {
                let root = self.s.db.wcroot();
                return r.map_err(|e| {
                    SvnError::from_io(format!("cannot sync '{}'", root.display()), &e)
                });
            }
            if self.deadline.is_some_and(|d| Instant::now() >= d) {
                self.poisoned = true;
                return Err(SvnError::new(
                    ErrorCode::Cancelled,
                    "cancelled while waiting for the disk; the next update finishes the work",
                ));
            }
        }
    }

    /// Waits for the staged batch's barrier, commits it in one transaction (with the completion
    /// of the work that ran before the barrier started) and runs the work it queued.
    fn commit_staged(&mut self) -> Result<()> {
        let Some(staged) = self.staged.take() else {
            return Ok(());
        };
        let p = staged.pending;
        let has_ops = !p.ops.is_empty();
        let committed = (|| {
            if let Some(ticket) = &staged.ticket {
                self.await_barrier(ticket)?;
            }
            if has_ops || !staged.settle.is_empty() {
                let settle = staged.settle;
                self.s.db.with_txn(|t| {
                    t.complete_work(&settle)?;
                    for op in p.ops {
                        op(t)?;
                    }
                    Ok(())
                })?;
            }
            Ok(())
        })();
        if let Err(e) = committed {
            self.poisoned = true;
            return Err(e);
        }
        if has_ops {
            self.out
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .committed
                .extend(p.nodes);
            // Committed: the queue owns the merge results now (it removes them after installing).
            for tmp in p.keep {
                let _ = tmp.keep();
            }
            if !p.removed.is_empty() {
                failpoint!("update.after_delete_txn");
            }
            failpoint!("update.after_txn_before_queue");
            let carried = std::mem::take(&mut self.unsettled);
            self.unsettled =
                crate::wc::run_work_queue_unsettled(&self.s.db, &self.s.cancel, carried)?;
        }
        Ok(())
    }

    /// Commits everything first when a node that is not committed yet (pending or staged)
    /// removed `wc` (see [`Batch::touches`]).
    pub(super) fn settle(&mut self, wc: &RelPath) -> Result<()> {
        let staged = self
            .staged
            .as_ref()
            .is_some_and(|s| s.pending.removed.contains(wc));
        if staged || self.batch.touches(wc) {
            self.drain()?;
        }
        Ok(())
    }

    pub(super) fn wc_path(&self, p: &RelPath) -> Result<RelPath> {
        wc::join_rel(&self.s.anchor, p)
    }

    fn dir(&self, id: DirId) -> Result<&DirState> {
        self.dirs
            .get(&id.0)
            .ok_or_else(|| SvnError::new(ErrorCode::Internal, "unknown directory handle"))
    }

    fn dir_mut(&mut self, id: DirId) -> Result<&mut DirState> {
        self.dirs
            .get_mut(&id.0)
            .ok_or_else(|| SvnError::new(ErrorCode::Internal, "unknown directory handle"))
    }

    pub(super) fn side(
        &self,
        node: Option<&Node>,
        new_repos: &str,
        kind: NodeKind,
    ) -> (Option<Side>, Option<Side>) {
        let old = node.and_then(|n| {
            Some(Side {
                repos_relpath: n.repos_relpath.clone()?,
                rev: n.revision?,
                kind: n.kind,
            })
        });
        let new = Side {
            repos_relpath: new_repos.to_owned(),
            rev: self.target_rev.map_or(0, |r| r.0),
            kind,
        };
        (old, Some(new))
    }

    /// Records a tree conflict on `wc` and remembers that the path was left alone.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_tree_conflict(
        &mut self,
        wc: &RelPath,
        base: Option<&Node>,
        new_repos: &str,
        kind: NodeKind,
        reason: TreeReason,
        action: TreeAction,
        copy_base: bool,
    ) -> Result<()> {
        let (old, new) = self.side(base, new_repos, kind);
        let info = conflict_record::operation_info(
            self.s.switch_relpath.is_some(),
            &self.s.repos_root,
            &self.s.uuid,
            old.as_ref(),
            new.as_ref(),
        );
        let path = wc.clone();
        self.defer(
            wc,
            Box::new(move |t| {
                if copy_base {
                    t.convert_base_to_copy(&path)?;
                }
                conflict_record::record(t, &path, &info, vec![NewConflict::Tree(reason, action)])
            }),
        )?;
        self.skipped.push(wc.clone());
        self.emit(wc, kind, UpdateAction::TreeConflicted);
        Ok(())
    }

    /// When `delete_entry` of this drive raised a tree conflict on `wc`, the add of `kind` that
    /// follows makes it a replacement, like svn's `add_directory`/`add_file`: the conflict
    /// becomes "local <reason>, incoming replace" (left side the deleted node, right side the
    /// added one) and the new node only goes into BASE, shadowed: the local node stays as it
    /// is, on disk too. Returns the op depth whose layer the new subtree extends with
    /// `base-deleted` rows: that of the local working node, or 0 when there is none (deleted
    /// locally: BASE only, as in svn).
    fn take_replacement(
        &mut self,
        wc: &RelPath,
        new_repos: &str,
        kind: NodeKind,
    ) -> Result<Option<u32>> {
        let Some((old, reason)) = self.deletion_conflicts.remove(wc) else {
            return Ok(None);
        };
        // The deletion's rows may still sit in the batch: commit them before this transaction,
        // which records the replacement on top of them and reads the working layer back.
        self.settle(wc)?;
        let (left, right) = self.side(Some(&old), new_repos, kind);
        let info = conflict_record::operation_info(
            self.s.switch_relpath.is_some(),
            &self.s.repos_root,
            &self.s.uuid,
            left.as_ref(),
            right.as_ref(),
        );
        self.s.db.with_txn(|t| {
            conflict_record::record(
                t,
                wc,
                &info,
                vec![NewConflict::Tree(reason, TreeAction::Replace)],
            )
        })?;
        let layer = self
            .s
            .db
            .read_node(wc)?
            .filter(|n| n.op_depth > 0 && n.presence != Presence::BaseDeleted)
            .map_or(0, |n| n.op_depth);
        Ok(Some(layer))
    }

    fn base_node_for(
        &self,
        kind: NodeKind,
        presence: Presence,
        new_repos: &str,
        old: Option<&Node>,
        depth: Option<Depth>,
    ) -> Result<BaseNode> {
        Ok(BaseNode {
            kind,
            presence,
            repos_root_url: self.s.repos_root.clone(),
            repos_uuid: self.s.uuid.clone(),
            repos_relpath: new_repos.to_owned(),
            revision: self.rev()?,
            changed_rev: old.and_then(|n| n.changed_rev),
            changed_date: old.and_then(|n| n.changed_date),
            changed_author: old.and_then(|n| n.changed_author.clone()),
            depth,
            properties: Some(Vec::new()),
            checksum: None,
            symlink_target: None,
            translated_size: None,
            last_mod_time: None,
            inherited_props: None,
            dav_cache: None,
        })
    }

    /// A `not-present` BASE row at the target revision: the node does not exist there.
    pub(super) fn not_present_node(&self, kind: NodeKind, repos: &str) -> Result<BaseNode> {
        let mut node = self.base_node_for(kind, Presence::NotPresent, repos, None, None)?;
        node.properties = None;
        Ok(node)
    }

    /// What the disk holds at `wc`.
    fn disk_kind(&self, wc: &RelPath) -> Result<Option<NodeKind>> {
        let path = wc::abs_path(&self.s.db, wc)?;
        Ok(match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_dir() => Some(NodeKind::Dir),
            Ok(m) if m.file_type().is_symlink() => Some(NodeKind::Symlink),
            Ok(_) => Some(NodeKind::File),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(SvnError::from_io(format!("cannot stat '{wc}'"), &e)),
        })
    }

    fn open_dir_state(&mut self, wc: RelPath, parent: Option<ParentInfo>) -> Result<(u32, bool)> {
        let base = self.s.db.read_base(&wc)?;
        let cur = self.s.db.read_node(&wc)?;
        let base = base
            .filter(|n| n.kind == NodeKind::Dir && n.presence.is_present())
            .ok_or_else(|| corrupt(format!("directory '{wc}' is not in the working copy base")))?;
        let was_incomplete = base.presence == Presence::Incomplete;
        let is_switch_root = self.s.switch_relpath.is_some() && wc == self.target_wc;
        let p = parent.unwrap_or_else(ParentInfo::anchor);
        let relocating = is_switch_root || p.relocating;
        let name = wc.file_name().unwrap_or("");
        let new_repos = if is_switch_root {
            self.s.switch_relpath.clone().unwrap_or_default()
        } else if relocating {
            repos_child(&p.repos, name)
        } else {
            base.repos_relpath
                .clone()
                .unwrap_or_else(|| repos_child(&p.repos, name))
        };
        let in_target = wc.starts_with(&self.target_wc);
        let mut skipped = p.skipped;
        let mut tree_conflict = None;
        let mut shadow = if skipped { None } else { p.shadow };
        let mut obstructed = false;
        if !skipped && in_target {
            if self.s.db.conflict(&wc)?.is_some() {
                skipped = true;
                shadow = None;
                self.skipped.push(wc.clone());
                self.emit(&wc, NodeKind::Dir, UpdateAction::Skipped);
            } else if shadow.is_none()
                && let Some(cur) = cur.as_ref().filter(|c| c.op_depth > 0)
            {
                // Deleted or replaced locally: the drive goes on below with BASE only (svn's
                // `shadowed` directory), the working layer stays as it is and the tree
                // conflict is recorded once, on this directory, when it closes.
                let reason = if cur.presence == Presence::BaseDeleted {
                    TreeReason::Deleted
                } else {
                    TreeReason::Replaced
                };
                tree_conflict = Some(PendingTreeConflict {
                    reason,
                    action: TreeAction::Edit,
                });
                shadow = Some(cur.op_depth);
            } else if shadow.is_none() && self.dir_obstructed(&wc)? {
                tree_conflict = Some(PendingTreeConflict {
                    reason: TreeReason::Obstructed,
                    action: TreeAction::Edit,
                });
                obstructed = true;
            }
        }
        let mut depth = base
            .depth
            .filter(|d| *d != Depth::Unknown)
            .unwrap_or(Depth::Infinity);
        // `--set-depth`: upgrading to infinity changes the depth of every directory, upgrading
        // to something else only that of the target (svn's `close_directory`).
        if self.s.sticky_depth && in_target && !skipped {
            let req = concrete(self.s.depth);
            if req == Depth::Infinity || (wc == self.target_wc && rank(req) > rank(depth)) {
                depth = req;
            }
        }
        // What the drive may touch below: the requested depth at the target, one level less
        // below an `immediates` parent, and never more than the depth already recorded.
        let requested = if wc == self.target_wc {
            concrete(self.s.depth)
        } else if in_target && p.limit == Depth::Immediates {
            Depth::Empty
        } else {
            Depth::Infinity
        };
        let limit = narrower(requested, depth);
        let not_present_on_drop = rank(requested) < rank(depth);
        if in_target && !skipped {
            let rev = self.rev()?;
            let reloc = relocating.then(|| new_repos.clone());
            let path = wc.clone();
            self.defer_incomplete(
                &wc,
                Box::new(move |t| t.mark_incomplete(&path, rev, reloc.as_deref())),
            )?;
        }
        let id = self.alloc();
        self.dirs.insert(
            id,
            DirState {
                wc,
                added: false,
                fresh: false,
                in_target,
                skipped,
                new_repos,
                relocating,
                base: Some(base),
                depth,
                limit,
                not_present_on_drop,
                shadow,
                obstructed,
                was_incomplete,
                prop_changes: Vec::new(),
                entry: EntryProps::default(),
                tree_conflict,
            },
        );
        Ok((id, skipped))
    }

    /// Whether the versioned directory `wc` is replaced on disk by a file or a symlink (to
    /// anything: the link is never followed). Only called where every parent of `wc` is a real
    /// directory (the directories above it are neither obstructed nor shadowed).
    fn dir_obstructed(&self, wc: &RelPath) -> Result<bool> {
        if wc.is_root() {
            return Ok(false);
        }
        Ok(matches!(
            self.disk_kind(wc)?,
            Some(NodeKind::File | NodeKind::Symlink)
        ))
    }

    fn parent_info(&self, parent: DirId) -> Result<ParentInfo> {
        let p = self.dir(parent)?;
        Ok(ParentInfo {
            repos: p.new_repos.clone(),
            relocating: p.relocating,
            skipped: p.skipped,
            depth: p.depth,
            limit: if p.in_target {
                p.limit
            } else {
                Depth::Infinity
            },
            not_present_on_drop: p.in_target && p.not_present_on_drop,
            // Below an obstructed directory: BASE only (svn's `shadowed` children of an
            // `edit_obstructed` directory).
            shadow: p.shadow.or(p.obstructed.then_some(0)),
            // A shadowed child keeps local rows below it (see `DirState::fresh`).
            fresh: p.fresh && p.shadow.is_none() && !p.obstructed,
        })
    }

    /// A directory the drive leaves alone (outside the requested depth): everything below it
    /// is ignored and nothing is recorded.
    fn dropped_dir(&mut self, wc: RelPath) -> DirId {
        let id = self.alloc();
        self.dirs.insert(
            id,
            DirState {
                wc,
                added: false,
                fresh: false,
                in_target: true,
                skipped: true,
                new_repos: String::new(),
                relocating: false,
                base: None,
                depth: Depth::Empty,
                limit: Depth::Empty,
                not_present_on_drop: false,
                shadow: None,
                obstructed: false,
                was_incomplete: false,
                prop_changes: Vec::new(),
                entry: EntryProps::default(),
                tree_conflict: None,
            },
        );
        DirId(id)
    }

    /// Records a `not-present` row for a child the server adds outside the requested depth, so
    /// the parent at the new revision does not claim to have it (svn issue #3569).
    fn add_dropped(&mut self, wc: &RelPath, p: &ParentInfo, kind: NodeKind) -> Result<()> {
        self.settle(wc)?;
        if !p.not_present_on_drop || self.s.db.read_base(wc)?.is_some() {
            return Ok(());
        }
        let repos = repos_child(&p.repos, wc.file_name().unwrap_or(""));
        let node = self.not_present_node(kind, &repos)?;
        let path = wc.clone();
        self.defer(wc, Box::new(move |t| t.base_add(&path, &node)))
    }
}

/// What a child needs from its open parent directory.
#[derive(Debug, Clone)]
pub(super) struct ParentInfo {
    repos: String,
    relocating: bool,
    skipped: bool,
    depth: Depth,
    limit: Depth,
    not_present_on_drop: bool,
    shadow: Option<u32>,
    /// See [`DirState::fresh`].
    fresh: bool,
}

impl ParentInfo {
    /// Stand-in for the parent of the anchor.
    fn anchor() -> Self {
        Self {
            repos: String::new(),
            relocating: false,
            skipped: false,
            depth: Depth::Infinity,
            limit: Depth::Infinity,
            not_present_on_drop: false,
            shadow: None,
            fresh: false,
        }
    }

    /// The drive applies to a child of `kind` below this directory.
    fn admits(&self, kind: NodeKind) -> bool {
        match self.limit {
            Depth::Empty => false,
            Depth::Files => kind != NodeKind::Dir,
            _ => true,
        }
    }
}

/// `op_depth` of a working layer rooted at `p` (number of path components).
fn relpath_depth(p: &RelPath) -> u32 {
    if p.is_root() {
        0
    } else {
        u32::try_from(p.as_str().split('/').count()).unwrap_or(u32::MAX)
    }
}

/// `Unknown` (and the meaningless `Exclude`) as the full depth.
fn concrete(d: Depth) -> Depth {
    match d {
        Depth::Unknown | Depth::Exclude => Depth::Infinity,
        d => d,
    }
}

fn rank(d: Depth) -> u8 {
    match concrete(d) {
        Depth::Empty => 0,
        Depth::Files => 1,
        Depth::Immediates => 2,
        _ => 3,
    }
}

fn narrower(a: Depth, b: Depth) -> Depth {
    if rank(a) <= rank(b) {
        concrete(a)
    } else {
        concrete(b)
    }
}

fn apply_entry_prop(entry: &mut EntryProps, change: &PropChange) {
    let text = change
        .value
        .as_ref()
        .and_then(|v| std::str::from_utf8(v).ok())
        .map(str::to_owned);
    match change.name.as_str() {
        "svn:entry:committed-rev" => entry.rev = text.and_then(|t| t.parse().ok()),
        "svn:entry:committed-date" => entry.date = text.and_then(|t| parse_svn_date(&t)),
        "svn:entry:last-author" => entry.author = text,
        _ => {}
    }
}

impl DeltaEditor for UpdateEditor {
    fn set_target_revision(&mut self, rev: Revnum) -> Result<()> {
        self.target_rev = Some(rev);
        self.out
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .revision = Some(rev);
        Ok(())
    }

    fn open_root(&mut self, _base_rev: Option<Revnum>) -> Result<DirId> {
        let wc = self.s.anchor.clone();
        let (id, _) = self.open_dir_state(wc, None)?;
        Ok(DirId(id))
    }

    fn delete_entry(&mut self, path: &RelPath, _rev: Option<Revnum>, parent: DirId) -> Result<()> {
        self.s.cancel.check()?;
        let p = self.parent_info(parent)?;
        if p.skipped {
            return Ok(());
        }
        let wc = self.wc_path(path)?;
        let base = self.s.db.read_base(&wc)?;
        // A file external is not part of its parent's tree (it is never reported), so the
        // parent's drive leaves it alone.
        if base.as_ref().is_some_and(|b| b.file_external) {
            return Ok(());
        }
        let kind = base.map_or(NodeKind::File, |b| b.kind);
        if !p.admits(kind) {
            return Ok(());
        }
        if p.shadow.is_some() {
            // Below a shadowed directory only BASE goes away; the working layer and the disk
            // stay, and the conflict is the one of the shadowed root.
            //
            // Deliberate divergence from svn 1.15: when the directory was replaced by a copy and
            // the user modified the copied file the server deletes, svn removes that modified
            // file from disk (it then shows as missing). We never touch the disk here, so the
            // user's edit survives; wc.db ends up identical to svn's. Do not "fix" this to match
            // svn. Pinned by `incoming_delete_in_copy_replaced_dir_keeps_modified_file_unlike_svn`.
            let path = wc.clone();
            self.defer(&wc, Box::new(move |t| t.base_remove(&path)))?;
            self.batch.removed(wc);
            return Ok(());
        }
        self.delete_node(&wc, parent)
    }

    fn add_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        _copy_from: Option<(&str, Revnum)>,
    ) -> Result<DirId> {
        self.s.cancel.check()?;
        let wc = self.wc_path(path)?;
        self.settle(&wc)?;
        let p = self.parent_info(parent)?;
        if !p.skipped && !p.admits(NodeKind::Dir) {
            self.add_dropped(&wc, &p, NodeKind::Dir)?;
            return Ok(self.dropped_dir(wc));
        }
        let name = wc.file_name().unwrap_or("").to_owned();
        let new_repos = repos_child(&p.repos, &name);
        // Below an `immediates` level a new directory starts out empty, as in svn.
        let depth = if p.limit == Depth::Immediates || p.depth == Depth::Immediates {
            Depth::Empty
        } else {
            Depth::Infinity
        };
        let mut skipped = p.skipped;
        let mut was_incomplete = false;
        let mut shadow = p.shadow;
        let mut obstructed = false;
        let mut tree_conflict = None;
        let (existing, base) = if p.fresh {
            (None, None)
        } else {
            (self.s.db.read_node(&wc)?, self.s.db.read_base(&wc)?)
        };
        if !skipped
            && shadow.is_none()
            && let Some(layer) = self.take_replacement(&wc, &new_repos, NodeKind::Dir)?
        {
            shadow = Some(layer);
        } else if !skipped && shadow.is_none() {
            let disk = self.disk_kind(&wc)?;
            match (&existing, &base) {
                (_, Some(b)) if b.kind == NodeKind::Dir && b.presence.is_present() => {
                    // A resumed update re-sends directories it already created. One replaced on
                    // disk by a file or a symlink since is obstructed, as when it is opened.
                    was_incomplete = true;
                    if matches!(disk, Some(NodeKind::File | NodeKind::Symlink)) {
                        obstructed = true;
                        tree_conflict = Some(PendingTreeConflict {
                            reason: TreeReason::Obstructed,
                            action: TreeAction::Edit,
                        });
                    }
                }
                (Some(e), _) if e.op_depth > 0 && e.presence == Presence::Normal => {
                    self.record_tree_conflict(
                        &wc,
                        None,
                        &new_repos,
                        NodeKind::Dir,
                        TreeReason::Added,
                        TreeAction::Add,
                        false,
                    )?;
                    skipped = true;
                }
                _ if matches!(disk, Some(NodeKind::File | NodeKind::Symlink)) => {
                    self.record_tree_conflict(
                        &wc,
                        None,
                        &new_repos,
                        NodeKind::Dir,
                        TreeReason::Unversioned,
                        TreeAction::Add,
                        false,
                    )?;
                    skipped = true;
                }
                _ if disk == Some(NodeKind::Dir) && !self.s.allow_unversioned_obstructions => {
                    // An unversioned directory in the way: the server's tree goes into BASE
                    // under a `base-deleted` layer and the disk stays as it is, with a tree
                    // conflict "local dir unversioned, incoming dir add" (svn without --force).
                    shadow = Some(relpath_depth(&wc));
                    tree_conflict = Some(PendingTreeConflict {
                        reason: TreeReason::Unversioned,
                        action: TreeAction::Add,
                    });
                }
                _ => {}
            }
        }
        if !skipped {
            let node = self.base_node_for(
                NodeKind::Dir,
                Presence::Incomplete,
                &new_repos,
                base.as_ref(),
                Some(depth),
            )?;
            let work = if shadow.is_some() || obstructed {
                Vec::new()
            } else {
                vec![
                    WorkItem::DirInstall {
                        relpath: wc.as_str().to_owned(),
                    }
                    .to_skel(),
                ]
            };
            let path = wc.clone();
            self.defer_incomplete(
                &wc,
                Box::new(move |t| {
                    t.base_add(&path, &node)?;
                    if let Some(op_depth) = shadow.filter(|d| *d > 0) {
                        t.mark_base_deleted_at(&path, NodeKind::Dir, op_depth)?;
                    }
                    t.queue_work(&work)
                }),
            )?;
            if !was_incomplete && shadow.is_none() {
                self.emit(&wc, NodeKind::Dir, UpdateAction::Add);
            }
        }
        let id = self.alloc();
        self.dirs.insert(
            id,
            DirState {
                wc,
                added: !was_incomplete,
                // A shadowed directory keeps the local working rows below it.
                fresh: !was_incomplete && !skipped && shadow.is_none(),
                in_target: true,
                skipped,
                new_repos,
                relocating: p.relocating,
                base,
                depth,
                limit: depth,
                not_present_on_drop: false,
                shadow,
                obstructed,
                was_incomplete,
                prop_changes: Vec::new(),
                entry: EntryProps::default(),
                tree_conflict,
            },
        );
        Ok(DirId(id))
    }

    fn open_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        _base_rev: Option<Revnum>,
    ) -> Result<DirId> {
        self.s.cancel.check()?;
        let wc = self.wc_path(path)?;
        let info = self.parent_info(parent)?;
        if !info.skipped && !info.admits(NodeKind::Dir) {
            return Ok(self.dropped_dir(wc));
        }
        let (id, _) = self.open_dir_state(wc, Some(info))?;
        Ok(DirId(id))
    }

    fn change_dir_prop(&mut self, dir: DirId, change: PropChange) -> Result<()> {
        let d = self.dir_mut(dir)?;
        apply_entry_prop(&mut d.entry, &change);
        d.prop_changes.push(change);
        Ok(())
    }

    fn close_directory(&mut self, dir: DirId) -> Result<()> {
        self.s.cancel.check()?;
        let d = self
            .dirs
            .remove(&dir.0)
            .ok_or_else(|| SvnError::new(ErrorCode::Internal, "unknown directory handle"))?;
        self.finish_dir(d)
    }

    fn absent_directory(&mut self, path: &RelPath, parent: DirId) -> Result<()> {
        self.absent(path, parent, NodeKind::Dir)
    }

    fn add_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        _copy_from: Option<(&str, Revnum)>,
    ) -> Result<FileId> {
        self.s.cancel.check()?;
        let wc = self.wc_path(path)?;
        self.settle(&wc)?;
        let id = self.begin_file(wc, parent, true)?;
        Ok(FileId(id))
    }

    fn open_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        _base_rev: Option<Revnum>,
    ) -> Result<FileId> {
        self.s.cancel.check()?;
        let wc = self.wc_path(path)?;
        let id = self.begin_file(wc, parent, false)?;
        Ok(FileId(id))
    }

    fn apply_textdelta(
        &mut self,
        file: FileId,
        base_checksum: Option<Checksum>,
    ) -> Result<Box<dyn WindowSink>> {
        let fs_state = self
            .files
            .get(&file.0)
            .ok_or_else(|| SvnError::new(ErrorCode::Internal, "unknown file handle"))?;
        if fs_state.skipped {
            return Ok(Box::new(DiscardSink));
        }
        let slot: TextSlot = Arc::clone(&fs_state.text);
        let source = match fs_state.base.as_ref().and_then(|b| b.checksum.as_deref()) {
            Some(cs) => {
                let sha1 = Sha1Digest::from_svn_string(cs)
                    .ok_or_else(|| corrupt("malformed checksum in wc.db"))?;
                let store = self.s.db.pristine_store();
                let (row, file) = store.open_text_with_row(&sha1)?;
                if let Some(Checksum::Md5(expected)) = base_checksum
                    && row.md5.0 != expected
                {
                    return Err(corrupt(format!(
                        "base text of '{}' does not match the checksum the server expects",
                        fs_state.wc
                    ))
                    .with_svn_code(200_014));
                }
                BaseSource::File(file)
            }
            None => BaseSource::Empty(std::io::Cursor::new(Vec::new())),
        };
        let dir = tmp_dir(&self.s.db);
        if !self.tmp_ready {
            fs::create_dir_all(&dir)
                .map_err(|e| SvnError::from_io(format!("cannot create '{}'", dir.display()), &e))?;
            self.tmp_ready = true;
        }
        let tmp = tempfile::Builder::new()
            .prefix("update-")
            .tempfile_in(&dir)
            .map_err(|e| {
                SvnError::from_io(
                    format!("cannot create temp file in '{}'", dir.display()),
                    &e,
                )
            })?;
        let out = tmp
            .as_file()
            .try_clone()
            .map_err(|e| SvnError::from_io("cannot duplicate temp file handle", &e))?;
        Ok(Box::new(TextSink::new(
            source,
            out,
            tmp.into_temp_path(),
            slot,
        )))
    }

    fn change_file_prop(&mut self, file: FileId, change: PropChange) -> Result<()> {
        let f = self
            .files
            .get_mut(&file.0)
            .ok_or_else(|| SvnError::new(ErrorCode::Internal, "unknown file handle"))?;
        apply_entry_prop(&mut f.entry, &change);
        f.prop_changes.push(change);
        Ok(())
    }

    fn close_file(&mut self, file: FileId, text_checksum: Option<Checksum>) -> Result<()> {
        self.s.cancel.check()?;
        let f = self
            .files
            .remove(&file.0)
            .ok_or_else(|| SvnError::new(ErrorCode::Internal, "unknown file handle"))?;
        self.finish_file(f, text_checksum)
    }

    fn absent_file(&mut self, path: &RelPath, parent: DirId) -> Result<()> {
        self.absent(path, parent, NodeKind::File)
    }

    fn close_edit(&mut self) -> Result<()> {
        self.finish_edit()
    }

    fn abort_edit(&mut self) -> Result<()> {
        // Directories already carry `incomplete` markers and finished files are committed
        // atomically, so the working copy is restartable as it stands. Queued work stays queued.
        // Nodes prepared before the failure are complete: commit them so the next update has
        // less to fetch (on failure they are simply not committed).
        if self.s.cancel.is_cancelled() {
            // The user is waiting: commit what is durable within the grace period and leave the
            // rest (a barrier on a slow or busy disk can take long).
            self.deadline = Some(Instant::now() + CANCEL_GRACE);
        }
        if !self.finished
            && let Err(e) = self.drain()
        {
            tracing::debug!("could not commit the finished nodes of an aborted drive: {e}");
        }
        self.finished = true;
        self.files.clear();
        self.dirs.clear();
        Ok(())
    }
}

impl UpdateEditor {
    fn absent(&mut self, path: &RelPath, parent: DirId, kind: NodeKind) -> Result<()> {
        let p = self.parent_info(parent)?;
        if p.skipped || !p.admits(kind) || p.shadow.is_some() {
            return Ok(());
        }
        let wc = self.wc_path(path)?;
        let new_repos = repos_child(&p.repos, wc.file_name().unwrap_or(""));
        let node = self.base_node_for(kind, Presence::ServerExcluded, &new_repos, None, None)?;
        let path = wc.clone();
        self.defer(&path, Box::new(move |t| t.base_add(&wc, &node)))
    }

    fn begin_file(&mut self, wc: RelPath, parent: DirId, add: bool) -> Result<u32> {
        let p = self.parent_info(parent)?;
        let (p_repos, p_reloc) = (p.repos.clone(), p.relocating);
        let mut skipped = p.skipped;
        if !skipped && !p.admits(NodeKind::File) {
            if add {
                self.add_dropped(&wc, &p, NodeKind::File)?;
            }
            skipped = true;
        }
        let name = wc.file_name().unwrap_or("").to_owned();
        let base = if p.fresh {
            None
        } else {
            self.s
                .db
                .read_base(&wc)?
                .filter(|b| b.presence.is_present())
        };
        let switch_root = self
            .s
            .switch_relpath
            .clone()
            .filter(|_| wc == self.target_wc);
        if !skipped && wc != self.target_wc && base.as_ref().is_some_and(|b| b.file_external) {
            // The parent's drive never touches a file external (see `delete_entry`).
            skipped = true;
            self.skipped.push(wc.clone());
            self.emit(&wc, NodeKind::File, UpdateAction::Skipped);
        }
        let new_repos = if let Some(sw) = switch_root {
            sw
        } else if p_reloc || base.is_none() {
            repos_child(&p_repos, &name)
        } else {
            base.as_ref()
                .and_then(|b| b.repos_relpath.clone())
                .unwrap_or_else(|| repos_child(&p_repos, &name))
        };
        let mut tree_conflict = None;
        let mut adopt_existing = false;
        let mut shadow = if skipped { None } else { p.shadow };
        if !skipped && add && shadow.is_none() {
            shadow = self.take_replacement(&wc, &new_repos, NodeKind::File)?;
        }
        let replacing = shadow.is_some() && p.shadow.is_none();
        if !skipped && !replacing && !p.fresh && self.s.db.conflict(&wc)?.is_some() {
            skipped = true;
            self.skipped.push(wc.clone());
            self.emit(&wc, NodeKind::File, UpdateAction::Skipped);
        }
        if !skipped && shadow.is_none() {
            let cur = if p.fresh {
                None
            } else {
                self.s.db.read_node(&wc)?
            };
            match (&base, add) {
                (Some(b), _) if b.kind != NodeKind::File => {
                    return Err(corrupt(format!(
                        "'{wc}' changed kind in the working copy base"
                    )));
                }
                (None, false) => {
                    return Err(corrupt(format!(
                        "file '{wc}' is not in the working copy base"
                    )));
                }
                (None, true) => {
                    if cur
                        .as_ref()
                        .is_some_and(|c| c.op_depth > 0 && c.presence == Presence::Normal)
                    {
                        self.record_tree_conflict(
                            &wc,
                            None,
                            &new_repos,
                            NodeKind::File,
                            TreeReason::Added,
                            TreeAction::Add,
                            false,
                        )?;
                        skipped = true;
                    } else if self.disk_kind(&wc)?.is_some() {
                        if self.s.allow_unversioned_obstructions {
                            adopt_existing = true;
                        } else {
                            // The server's file becomes BASE, the local one stays on disk as
                            // an unversioned obstruction (recorded at close).
                            tree_conflict = Some(PendingTreeConflict {
                                reason: TreeReason::Unversioned,
                                action: TreeAction::Add,
                            });
                        }
                    }
                }
                (Some(_), _) => {
                    if let Some(c) = cur.as_ref().filter(|c| c.op_depth > 0) {
                        tree_conflict = Some(if c.presence == Presence::BaseDeleted {
                            PendingTreeConflict {
                                reason: TreeReason::Deleted,
                                action: TreeAction::Edit,
                            }
                        } else {
                            PendingTreeConflict {
                                reason: TreeReason::Replaced,
                                action: TreeAction::Edit,
                            }
                        });
                    }
                }
            }
        }
        let id = self.alloc();
        self.files.insert(
            id,
            FileState {
                wc,
                added: base.is_none(),
                skipped,
                new_repos,
                base,
                adopt_existing,
                prop_changes: Vec::new(),
                entry: EntryProps::default(),
                text: Arc::new(Mutex::new(None)),
                tree_conflict,
                shadow: if skipped { None } else { shadow },
            },
        );
        Ok(id)
    }
}

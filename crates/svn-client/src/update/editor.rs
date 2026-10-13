//! The update editor: applies a server drive to a working copy.
//!
//! Safety model (see the module docs of [`super`]): every directory is marked `incomplete` before
//! it changes, every file or directory is committed to wc.db in one transaction together with the
//! work items that make the disk match, and cancellation or an aborted drive leaves
//! `incomplete` markers so the next update resumes.

use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex, PoisonError};

use svn_types::{
    Checksum, DeltaEditor, Depth, DirId, ErrorCode, FileId, NodeKind, PropChange, RelPath, Result,
    Revnum, SvnError, WindowSink,
};
use svn_wc::conflicts::{TreeAction, TreeReason};
use svn_wc::db::{BaseNode, Node, Presence};
use svn_wc::pristine::Sha1Digest;
use svn_wc::workqueue::WorkItem;

use super::conflict_record::{self, NewConflict, Side};
use super::state::{
    DirState, EntryProps, FileState, Outcome, OutcomeHandle, PendingTreeConflict, TextSlot,
    UpdateSetup, parse_svn_date, tmp_dir,
};
use super::text_sink::{BaseSource, DiscardSink, TextSink};
use crate::ctx::{UpdateAction, UpdateEvent};
use crate::wc::{self, adapters, txn};

/// Failpoint hook; compiled out without the `failpoints` feature.
#[cfg(feature = "failpoints")]
macro_rules! failpoint {
    ($name:literal) => {
        fail::fail_point!($name, |_| Err(SvnError::new(
            ErrorCode::Internal,
            concat!("failpoint ", $name)
        )));
    };
}

#[cfg(not(feature = "failpoints"))]
macro_rules! failpoint {
    ($name:literal) => {};
}
pub(super) use failpoint;

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
    old_target_repos: Option<String>,
    finished: bool,
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
                old_target_repos,
                finished: false,
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

    pub(super) fn emit(&self, path: &RelPath, kind: NodeKind, action: UpdateAction) {
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

    pub(super) fn wc_path(&self, p: &RelPath) -> Result<RelPath> {
        wc::join_rel(&self.s.anchor, p)
    }

    pub(super) fn run_queue(&self) -> Result<()> {
        self.s.cancel.check()?;
        adapters::run_work_queue(&self.s.db, &self.s.cancel).map(|_| ())
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

    pub(super) fn side(&self, node: Option<&Node>, new_repos: &str, kind: NodeKind) -> (Option<Side>, Option<Side>) {
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
        let wc_id = self.s.db.wc_id();
        self.s.db.with_write(|tx| {
            if copy_base {
                txn::convert_base_to_copy(tx, wc_id, wc)?;
            }
            conflict_record::record(tx, wc_id, wc, &info, vec![NewConflict::Tree(reason, action)])
        })?;
        self.skipped.push(wc.clone());
        self.emit(wc, kind, UpdateAction::TreeConflicted);
        Ok(())
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

    fn open_dir_state(
        &mut self,
        wc: RelPath,
        parent: Option<(String, bool, bool, Depth)>,
    ) -> Result<(u32, bool)> {
        let base = self.s.db.read_base(&wc)?;
        let cur = self.s.db.read_node(&wc)?;
        let base = base
            .filter(|n| n.kind == NodeKind::Dir && n.presence.is_present())
            .ok_or_else(|| corrupt(format!("directory '{wc}' is not in the working copy base")))?;
        let is_switch_root = self.s.switch_relpath.is_some() && wc == self.target_wc;
        let (p_repos, p_reloc, p_skipped, p_depth) = parent.unwrap_or_default_parent();
        let relocating = is_switch_root || p_reloc;
        let name = wc.file_name().unwrap_or("");
        let new_repos = if is_switch_root {
            self.s.switch_relpath.clone().unwrap_or_default()
        } else if relocating {
            repos_child(&p_repos, name)
        } else {
            base.repos_relpath.clone().unwrap_or_else(|| repos_child(&p_repos, name))
        };
        let in_target = wc.starts_with(&self.target_wc);
        let mut skipped = p_skipped;
        let mut tree_conflict = None;
        if !skipped && in_target {
            if self.s.db.conflict(&wc)?.is_some() {
                skipped = true;
                self.skipped.push(wc.clone());
                self.emit(&wc, NodeKind::Dir, UpdateAction::Skipped);
            } else if let Some(cur) = cur.as_ref().filter(|c| c.op_depth > 0) {
                if cur.presence == Presence::BaseDeleted {
                    tree_conflict = Some(PendingTreeConflict {
                        reason: TreeReason::Deleted,
                        action: TreeAction::Edit,
                    });
                } else {
                    tree_conflict = Some(PendingTreeConflict {
                        reason: TreeReason::Replaced,
                        action: TreeAction::Edit,
                    });
                }
                skipped = true;
            }
        }
        let depth = if wc == self.target_wc && self.s.depth != Depth::Unknown {
            self.s.depth
        } else {
            base.depth.filter(|d| *d != Depth::Unknown).unwrap_or(Depth::Infinity)
        };
        let _ = p_depth;
        if in_target && !skipped {
            let rev = self.rev()?;
            let wc_id = self.s.db.wc_id();
            let reloc = relocating.then_some(new_repos.as_str());
            self.s
                .db
                .with_write(|tx| txn::mark_incomplete(tx, wc_id, &wc, rev, reloc))?;
        }
        let id = self.alloc();
        self.dirs.insert(
            id,
            DirState {
                wc,
                added: false,
                in_target,
                skipped,
                new_repos,
                relocating,
                base: Some(base),
                depth,
                prop_changes: Vec::new(),
                entry: EntryProps::default(),
                tree_conflict,
            },
        );
        Ok((id, skipped))
    }

    fn parent_info(&self, parent: DirId) -> Result<(String, bool, bool, Depth)> {
        let p = self.dir(parent)?;
        Ok((p.new_repos.clone(), p.relocating, p.skipped, p.depth))
    }
}

trait DefaultParent {
    fn unwrap_or_default_parent(self) -> (String, bool, bool, Depth);
}

impl DefaultParent for Option<(String, bool, bool, Depth)> {
    fn unwrap_or_default_parent(self) -> (String, bool, bool, Depth) {
        self.unwrap_or((String::new(), false, false, Depth::Infinity))
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
        if self.dir(parent)?.skipped {
            return Ok(());
        }
        let wc = self.wc_path(path)?;
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
        let (p_repos, p_reloc, p_skipped, p_depth) = self.parent_info(parent)?;
        let name = wc.file_name().unwrap_or("").to_owned();
        let new_repos = repos_child(&p_repos, &name);
        let depth = if p_depth == Depth::Immediates {
            Depth::Empty
        } else {
            Depth::Infinity
        };
        let rev = self.rev()?;
        let wc_id = self.s.db.wc_id();
        let mut skipped = p_skipped;
        let mut was_incomplete = false;
        let existing = self.s.db.read_node(&wc)?;
        let base = self.s.db.read_base(&wc)?;
        if !skipped {
            let disk = self.disk_kind(&wc)?;
            match (&existing, &base) {
                (_, Some(b)) if b.kind == NodeKind::Dir && b.presence.is_present() => {
                    // A resumed update re-sends directories it already created.
                    was_incomplete = true;
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
            let work = [WorkItem::DirInstall {
                relpath: wc.as_str().to_owned(),
            }
            .to_skel()];
            self.s.db.with_write(|tx| {
                txn::base_add(tx, wc_id, &wc, &node)?;
                txn::queue_work(tx, &work)
            })?;
            self.run_queue()?;
            if !was_incomplete {
                self.emit(&wc, NodeKind::Dir, UpdateAction::Add);
            }
        }
        let _ = rev;
        let id = self.alloc();
        self.dirs.insert(
            id,
            DirState {
                wc,
                added: !was_incomplete,
                in_target: true,
                skipped,
                new_repos,
                relocating: p_reloc,
                base,
                depth,
                prop_changes: Vec::new(),
                entry: EntryProps::default(),
                tree_conflict: None,
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
                let store = adapters::pristine_store(&self.s.db);
                if let Some(Checksum::Md5(expected)) = base_checksum
                    && let Some(row) = svn_wc::pristine::PristineIndex::lookup(store.index(), &sha1)?
                    && row.md5.0 != expected
                {
                    return Err(corrupt(format!(
                        "base text of '{}' does not match the checksum the server expects",
                        fs_state.wc
                    ))
                    .with_svn_code(200_014));
                }
                BaseSource::File(store.open_text(&sha1)?)
            }
            None => BaseSource::Empty(std::io::Cursor::new(Vec::new())),
        };
        let dir = tmp_dir(&self.s.db);
        fs::create_dir_all(&dir)
            .map_err(|e| SvnError::from_io(format!("cannot create '{}'", dir.display()), &e))?;
        let tmp = tempfile::Builder::new()
            .prefix("update-")
            .tempfile_in(&dir)
            .map_err(|e| SvnError::from_io(format!("cannot create temp file in '{}'", dir.display()), &e))?;
        let out = tmp
            .as_file()
            .try_clone()
            .map_err(|e| SvnError::from_io("cannot duplicate temp file handle", &e))?;
        Ok(Box::new(TextSink::new(source, out, tmp.into_temp_path(), slot)))
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
        self.finished = true;
        self.files.clear();
        self.dirs.clear();
        Ok(())
    }
}

impl UpdateEditor {
    fn absent(&mut self, path: &RelPath, parent: DirId, kind: NodeKind) -> Result<()> {
        if self.dir(parent)?.skipped {
            return Ok(());
        }
        let wc = self.wc_path(path)?;
        let (p_repos, ..) = self.parent_info(parent)?;
        let new_repos = repos_child(&p_repos, wc.file_name().unwrap_or(""));
        let node = self.base_node_for(kind, Presence::ServerExcluded, &new_repos, None, None)?;
        let wc_id = self.s.db.wc_id();
        self.s
            .db
            .with_write(|tx| txn::base_add(tx, wc_id, &wc, &node))?;
        Ok(())
    }

    fn begin_file(&mut self, wc: RelPath, parent: DirId, add: bool) -> Result<u32> {
        let (p_repos, p_reloc, p_skipped, _) = self.parent_info(parent)?;
        let name = wc.file_name().unwrap_or("").to_owned();
        let base = self.s.db.read_base(&wc)?.filter(|b| b.presence.is_present());
        let new_repos = if p_reloc || base.is_none() {
            repos_child(&p_repos, &name)
        } else {
            base.as_ref()
                .and_then(|b| b.repos_relpath.clone())
                .unwrap_or_else(|| repos_child(&p_repos, &name))
        };
        let mut skipped = p_skipped;
        let mut tree_conflict = None;
        let mut adopt_existing = false;
        if !skipped && self.s.db.conflict(&wc)?.is_some() {
            skipped = true;
            self.skipped.push(wc.clone());
            self.emit(&wc, NodeKind::File, UpdateAction::Skipped);
        }
        if !skipped {
            let cur = self.s.db.read_node(&wc)?;
            match (&base, add) {
                (Some(b), _) if b.kind != NodeKind::File => {
                    return Err(corrupt(format!("'{wc}' changed kind in the working copy base")));
                }
                (None, false) => {
                    return Err(corrupt(format!("file '{wc}' is not in the working copy base")));
                }
                (None, true) => {
                    if cur.as_ref().is_some_and(|c| c.op_depth > 0 && c.presence == Presence::Normal) {
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
            },
        );
        Ok(id)
    }
}

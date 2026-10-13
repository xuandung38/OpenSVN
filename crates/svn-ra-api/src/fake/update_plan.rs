//! Turns a working-copy report plus repository snapshots into a linear list of editor operations.
//!
//! The plan is built synchronously (pure function of its inputs) and executed afterwards by
//! the async driver. Files are closed late (bounded number of simultaneously open files) and
//! always before their ancestor directory is closed, so editors see the same ordering a
//! parallel-download server produces.

use std::collections::{BTreeSet, HashMap, VecDeque};

use svn_types::{
    Depth, ErrorCode, PropChange, RelPath, RepoUrl, Reporter, Result, Revnum, SvnError,
};

use super::repo::{Node, NodeData, Tree};

/// One report entry.
#[derive(Debug, Clone)]
pub(super) struct ReportEntry {
    pub path: RelPath,
    pub rev: Revnum,
    pub start_empty: bool,
    pub deleted: bool,
}

/// Reporter that records entries.
#[derive(Debug, Default)]
pub(super) struct CollectingReporter {
    pub entries: Vec<ReportEntry>,
}

impl Reporter for CollectingReporter {
    fn set_path(
        &mut self,
        path: &RelPath,
        rev: Revnum,
        _depth: Depth,
        start_empty: bool,
        _lock: Option<&str>,
    ) -> Result<()> {
        self.entries.push(ReportEntry {
            path: path.clone(),
            rev,
            start_empty,
            deleted: false,
        });
        Ok(())
    }
    fn delete_path(&mut self, path: &RelPath) -> Result<()> {
        self.entries.push(ReportEntry {
            path: path.clone(),
            rev: Revnum(0),
            start_empty: true,
            deleted: true,
        });
        Ok(())
    }
    fn link_path(
        &mut self,
        path: &RelPath,
        _url: &RepoUrl,
        rev: Revnum,
        _depth: Depth,
        start_empty: bool,
        _lock: Option<&str>,
    ) -> Result<()> {
        self.entries.push(ReportEntry {
            path: path.clone(),
            rev,
            start_empty,
            deleted: false,
        });
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
    fn abort(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Slot numbers are plan-local; the executor maps them to the ids the editor returns.
pub(super) type DirSlot = usize;
pub(super) type FileSlot = usize;

#[derive(Debug)]
pub(super) enum Op {
    SetTarget(Revnum),
    OpenRoot(Option<Revnum>),
    Dir {
        slot: DirSlot,
        parent: DirSlot,
        path: RelPath,
        add: bool,
        base: Option<Revnum>,
    },
    DirProp(DirSlot, PropChange),
    CloseDir(DirSlot),
    Delete {
        path: RelPath,
        rev: Option<Revnum>,
        parent: DirSlot,
    },
    File {
        slot: FileSlot,
        parent: DirSlot,
        path: RelPath,
        add: bool,
        base: Option<Revnum>,
    },
    FileProp(FileSlot, PropChange),
    Text(FileSlot, bytes::Bytes),
    CloseFile(FileSlot),
}

/// Inputs of planning.
pub(super) struct PlanInput<'a> {
    pub target_rev: Revnum,
    pub target_tree: &'a Tree,
    pub base_trees: &'a HashMap<Revnum, Tree>,
    pub entries: &'a [ReportEntry],
    /// Anchor-relative path being updated.
    pub anchor_target: RelPath,
    /// Where `anchor_target` lives in the target tree (differs for switch).
    pub tree_target: RelPath,
    pub send_text: bool,
    pub max_open_files: usize,
}

struct Planner<'a> {
    input: &'a PlanInput<'a>,
    ops: Vec<Op>,
    next_dir: DirSlot,
    next_file: FileSlot,
    pending: VecDeque<(FileSlot, RelPath)>,
}

fn remap(p: &RelPath, from: &RelPath, to: &RelPath) -> Result<RelPath> {
    let rest = p
        .as_str()
        .get(from.as_str().len()..)
        .unwrap_or("")
        .trim_start_matches('/');
    if rest.is_empty() {
        Ok(to.clone())
    } else if to.is_root() {
        RelPath::new(rest)
    } else {
        RelPath::new(&format!("{to}/{rest}"))
    }
}

fn prop_changes(base: Option<&Node>, new: &Node) -> Vec<PropChange> {
    let mut out = Vec::new();
    for (k, v) in &new.props {
        if base.and_then(|b| b.props.get(k)) != Some(v) {
            out.push(PropChange {
                name: k.clone(),
                value: Some(v.clone()),
            });
        }
    }
    if let Some(b) = base {
        for k in b.props.keys().filter(|k| !new.props.contains_key(*k)) {
            out.push(PropChange {
                name: k.clone(),
                value: None,
            });
        }
    }
    out
}

/// Builds the operation list.
pub(super) fn plan(input: &PlanInput<'_>) -> Result<Vec<Op>> {
    let mut p = Planner {
        input,
        ops: vec![Op::SetTarget(input.target_rev)],
        next_dir: 1,
        next_file: 0,
        pending: VecDeque::new(),
    };
    p.run()?;
    Ok(p.ops)
}

impl Planner<'_> {
    fn governing(&self, path: &RelPath) -> Option<&ReportEntry> {
        self.input
            .entries
            .iter()
            .filter(|e| path.starts_with(&e.path))
            .max_by_key(|e| e.path.as_str().len())
    }

    fn base_node(&self, path: &RelPath, empty_base: bool) -> Option<(&Node, Revnum)> {
        if empty_base {
            return None;
        }
        let e = self.governing(path)?;
        if e.deleted || e.start_empty {
            return None;
        }
        self.input
            .base_trees
            .get(&e.rev)?
            .get(path)
            .map(|n| (n, e.rev))
    }

    fn new_node(&self, path: &RelPath) -> Result<Option<&Node>> {
        let t = remap(path, &self.input.anchor_target, &self.input.tree_target)?;
        Ok(self.input.target_tree.get(&t))
    }

    fn run(&mut self) -> Result<()> {
        let anchor = self.input.anchor_target.clone();
        let root_rev = self
            .governing(&RelPath::root())
            .or_else(|| self.governing(&anchor))
            .map(|e| e.rev)
            .ok_or_else(|| {
                SvnError::new(
                    ErrorCode::Corrupt,
                    "report does not cover the update target",
                )
            })?;
        self.ops.push(Op::OpenRoot(Some(root_rev)));
        // Ancestors from the root down to the anchor's parent.
        let mut chain = Vec::new();
        let mut cur = anchor.parent();
        while let Some(p) = cur {
            cur = p.parent();
            chain.push(p);
        }
        chain.reverse();
        let mut parent = 0;
        let mut opened = Vec::new();
        for anc in chain.into_iter().filter(|p| !p.is_root()) {
            let slot = self.alloc_dir();
            let base = self.governing(&anc).map(|e| e.rev);
            self.ops.push(Op::Dir {
                slot,
                parent,
                path: anc,
                add: false,
                base,
            });
            opened.push(slot);
            parent = slot;
        }
        if anchor.is_root() {
            self.children(&anchor, 0, false)?;
            let base = self.base_node(&anchor, false).map(|(n, _)| n);
            if let Some(t) = self.input.target_tree.get(&self.input.tree_target) {
                for c in prop_changes(base, t) {
                    self.ops.push(Op::DirProp(0, c));
                }
            }
        } else {
            let found =
                self.new_node(&anchor)?.is_some() || self.base_node(&anchor, false).is_some();
            if !found {
                return Err(SvnError::new(
                    ErrorCode::PathNotFound,
                    format!("{anchor} does not exist"),
                ));
            }
            self.entry(parent, &anchor, false)?;
        }
        for slot in opened.into_iter().rev() {
            self.close_dir(slot, &RelPath::root());
        }
        self.close_dir(0, &RelPath::root());
        Ok(())
    }

    fn alloc_dir(&mut self) -> DirSlot {
        let s = self.next_dir;
        self.next_dir += 1;
        s
    }

    fn alloc_file(&mut self) -> FileSlot {
        let s = self.next_file;
        self.next_file += 1;
        s
    }

    /// Closes pending files below `dir`, then the directory (files never outlive their parent).
    fn close_dir(&mut self, slot: DirSlot, dir: &RelPath) {
        let (below, rest): (Vec<_>, Vec<_>) = self
            .pending
            .drain(..)
            .partition(|(_, p)| p.starts_with(dir));
        self.pending = rest.into();
        for (s, _) in below {
            self.ops.push(Op::CloseFile(s));
        }
        self.ops.push(Op::CloseDir(slot));
    }

    fn children(&mut self, dir: &RelPath, slot: DirSlot, empty_base: bool) -> Result<()> {
        let mut names = BTreeSet::new();
        if !empty_base {
            let rev = self
                .governing(dir)
                .filter(|e| !e.deleted && !e.start_empty)
                .map(|e| e.rev);
            if let Some(t) = rev.and_then(|r| self.input.base_trees.get(&r)) {
                names.extend(
                    super::repo::children(t, dir)
                        .filter_map(|(k, _)| k.file_name().map(str::to_owned)),
                );
            }
        }
        let tdir = remap(dir, &self.input.anchor_target, &self.input.tree_target)?;
        names.extend(
            super::repo::children(self.input.target_tree, &tdir)
                .filter_map(|(k, _)| k.file_name().map(str::to_owned)),
        );
        for name in names {
            let cp = dir.child(&name)?;
            self.entry(slot, &cp, empty_base)?;
        }
        Ok(())
    }

    fn entry(&mut self, parent: DirSlot, cp: &RelPath, empty_base: bool) -> Result<()> {
        let base = self.base_node(cp, empty_base).map(|(n, r)| (n.clone(), r));
        let new = self.new_node(cp)?.cloned();
        match (base, new) {
            (None, None) => Ok(()),
            (Some((_, r)), None) => {
                self.ops.push(Op::Delete {
                    path: cp.clone(),
                    rev: Some(r),
                    parent,
                });
                Ok(())
            }
            (None, Some(t)) => self.add(parent, cp, &t),
            (Some((b, r)), Some(t)) if b.kind() != t.kind() => {
                self.ops.push(Op::Delete {
                    path: cp.clone(),
                    rev: Some(r),
                    parent,
                });
                self.add(parent, cp, &t)
            }
            (Some((b, r)), Some(t)) => match &t.data {
                NodeData::Dir => {
                    let slot = self.alloc_dir();
                    let mark = self.ops.len();
                    self.ops.push(Op::Dir {
                        slot,
                        parent,
                        path: cp.clone(),
                        add: false,
                        base: Some(r),
                    });
                    for c in prop_changes(Some(&b), &t) {
                        self.ops.push(Op::DirProp(slot, c));
                    }
                    self.children(cp, slot, false)?;
                    if self.ops.len() == mark + 1 {
                        self.ops.pop(); // nothing changed below: leave the directory unopened
                    } else {
                        self.close_dir(slot, cp);
                    }
                    Ok(())
                }
                NodeData::File(content) => {
                    let text_changed = b.data != t.data;
                    let props = prop_changes(Some(&b), &t);
                    if !text_changed && props.is_empty() {
                        return Ok(());
                    }
                    let slot = self.alloc_file();
                    self.ops.push(Op::File {
                        slot,
                        parent,
                        path: cp.clone(),
                        add: false,
                        base: Some(r),
                    });
                    for c in props {
                        self.ops.push(Op::FileProp(slot, c));
                    }
                    if text_changed && self.input.send_text {
                        self.ops.push(Op::Text(slot, content.clone()));
                    }
                    self.finish_file(slot, cp);
                    Ok(())
                }
            },
        }
    }

    fn add(&mut self, parent: DirSlot, cp: &RelPath, t: &Node) -> Result<()> {
        match &t.data {
            NodeData::Dir => {
                let slot = self.alloc_dir();
                self.ops.push(Op::Dir {
                    slot,
                    parent,
                    path: cp.clone(),
                    add: true,
                    base: None,
                });
                for c in prop_changes(None, t) {
                    self.ops.push(Op::DirProp(slot, c));
                }
                self.children(cp, slot, true)?;
                self.close_dir(slot, cp);
            }
            NodeData::File(content) => {
                let slot = self.alloc_file();
                self.ops.push(Op::File {
                    slot,
                    parent,
                    path: cp.clone(),
                    add: true,
                    base: None,
                });
                for c in prop_changes(None, t) {
                    self.ops.push(Op::FileProp(slot, c));
                }
                if self.input.send_text {
                    self.ops.push(Op::Text(slot, content.clone()));
                }
                self.finish_file(slot, cp);
            }
        }
        Ok(())
    }

    /// Keeps the file open (late close) until more than `max_open_files` are pending.
    fn finish_file(&mut self, slot: FileSlot, path: &RelPath) {
        self.pending.push_back((slot, path.clone()));
        while self.pending.len() > self.input.max_open_files {
            if let Some((s, _)) = self.pending.pop_front() {
                self.ops.push(Op::CloseFile(s));
            }
        }
    }
}

//! Local (no network) working copy operations of the fake engine.

use std::path::Path;

use opensvn_ipc::{
    CleanupRequest, Depth, FileKind, IgnoreAddRequest, IgnoreScope, ResolveChoice, ResolveRequest,
    RevertRequest,
};
use svn_types::{ErrorCode, RelPath, SvnError};

use super::FakeEngine;
use super::state::{FakeNode, Sched, rel};

fn bad_arg(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::Internal, msg)
}

impl FakeEngine {
    pub(super) fn do_changelist_set(
        &self,
        root: &Path,
        paths: &[String],
        name: Option<&str>,
    ) -> Result<(), SvnError> {
        if let Some(n) = name
            && (n.trim().is_empty() || n.len() > 100 || n.chars().any(char::is_control))
        {
            return Err(bad_arg("invalid changelist name"));
        }
        self.with_wc(root, |wc| {
            let targets = wc.expand_selection(paths)?;
            for p in targets {
                let node = wc.node_mut(&p)?;
                if node.kind == FileKind::File {
                    node.changelist = name.map(str::to_owned);
                }
            }
            Ok(())
        })
    }

    pub(super) fn do_ignore_add(
        &self,
        root: &Path,
        req: &IgnoreAddRequest,
    ) -> Result<(), SvnError> {
        let pattern = req.pattern.trim();
        if pattern.is_empty() || pattern.contains(['/', '\0', '\n', '\r']) {
            return Err(bad_arg("invalid ignore pattern"));
        }
        let dir = rel(&req.dir)?;
        self.with_wc(root, |wc| {
            let node = wc.node(&dir)?;
            match req.scope {
                // A property of the directory, committed like any other property change.
                IgnoreScope::SvnIgnore => {
                    if node.kind != FileKind::Dir || !node.is_versioned() {
                        return Err(bad_arg("svn:ignore can only be set on a versioned folder"));
                    }
                    let node = wc.node_mut(&dir)?;
                    let current = node.props.get("svn:ignore").cloned().unwrap_or_default();
                    if !current.lines().any(|l| l.trim() == pattern) {
                        let mut value = current;
                        if !value.is_empty() && !value.ends_with('\n') {
                            value.push('\n');
                        }
                        value.push_str(pattern);
                        value.push('\n');
                        node.props.insert("svn:ignore".to_owned(), value);
                    }
                }
                IgnoreScope::Local => {
                    if !wc.ignores.iter().any(|p| p == pattern) {
                        wc.ignores.push(pattern.to_owned());
                    }
                }
            }
            Ok(())
        })
    }

    pub(super) fn do_mark_move(&self, root: &Path, from: &str, to: &str) -> Result<(), SvnError> {
        let (from, to) = (rel(from)?, rel(to)?);
        self.with_wc(root, |wc| {
            wc.node(&from)?;
            wc.node(&to)?;
            wc.node_mut(&from)?.moved_to = Some(to.as_str().to_owned());
            wc.node_mut(&to)?.moved_from = Some(from.as_str().to_owned());
            Ok(())
        })
    }

    pub(super) fn do_resolve(&self, root: &Path, req: &ResolveRequest) -> Result<(), SvnError> {
        let path = rel(&req.path)?;
        self.with_wc(root, |wc| {
            let node = wc.node_mut(&path)?;
            if !node.conflicted && !node.tree_conflict {
                return Ok(());
            }
            match req.choice {
                ResolveChoice::Mine | ResolveChoice::Working => {}
                ResolveChoice::Theirs | ResolveChoice::Base => node.work = node.base.clone(),
            }
            node.conflicted = false;
            node.tree_conflict = false;
            Ok(())
        })
    }

    pub(super) fn do_props_set(
        &self,
        root: &Path,
        path: &str,
        name: &str,
        value: Option<&str>,
    ) -> Result<(), SvnError> {
        if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(bad_arg("invalid property name"));
        }
        let path = rel(path)?;
        self.with_wc(root, |wc| {
            let node = wc.node_mut(&path)?;
            match value {
                Some(v) => node.props.insert(name.to_owned(), v.to_owned()),
                None => node.props.remove(name),
            };
            Ok(())
        })
    }

    pub(super) fn do_add(&self, root: &Path, paths: &[String]) -> Result<(), SvnError> {
        self.with_wc(root, |wc| {
            let targets = wc.expand_selection(paths)?;
            for p in &targets {
                let node = wc.node(p)?;
                if node.is_versioned() {
                    return Err(bad_arg(format!("{p} is already under version control")));
                }
            }
            for p in targets {
                // Parents of an added node must be versioned too.
                let mut chain: Vec<RelPath> =
                    std::iter::successors(p.parent(), RelPath::parent).collect();
                chain.push(p.clone());
                for q in chain {
                    if let Some(n) = wc.nodes.get_mut(&q)
                        && n.sched == Sched::Unversioned
                    {
                        n.sched = Sched::Added;
                    }
                }
            }
            Ok(())
        })
    }

    pub(super) fn do_delete(
        &self,
        root: &Path,
        paths: &[String],
        keep_local: bool,
    ) -> Result<(), SvnError> {
        self.with_wc(root, |wc| {
            let targets = wc.expand_selection(paths)?;
            for p in &targets {
                if !wc.node(p)?.is_versioned() {
                    return Err(bad_arg(format!("{p} is not under version control")));
                }
            }
            for p in targets {
                if wc.node(&p)?.sched == Sched::Added {
                    if keep_local {
                        wc.node_mut(&p)?.sched = Sched::Unversioned;
                    } else {
                        wc.nodes.remove(&p);
                    }
                } else {
                    let n = wc.node_mut(&p)?;
                    n.sched = Sched::Deleted;
                    if !keep_local {
                        n.work = None;
                    }
                }
            }
            Ok(())
        })
    }

    pub(super) fn do_move(&self, root: &Path, from: &str, to: &str) -> Result<(), SvnError> {
        let (from, to) = (rel(from)?, rel(to)?);
        if from.is_root() || to.is_root() || to.starts_with(&from) {
            return Err(bad_arg("cannot move a path into itself"));
        }
        self.with_wc(root, |wc| {
            if wc.nodes.contains_key(&to) {
                return Err(SvnError::new(
                    ErrorCode::Internal,
                    format!("{to} already exists"),
                ));
            }
            if let Some(parent) = to.parent() {
                wc.node(&parent)?;
            }
            for p in wc.subtree(&from) {
                let suffix = p
                    .as_str()
                    .get(from.as_str().len()..)
                    .unwrap_or("")
                    .trim_start_matches('/');
                let target = if suffix.is_empty() {
                    to.clone()
                } else {
                    to.child(suffix)?
                };
                let src = wc.node_mut(&p)?;
                let mut copy = src.clone();
                src.sched = Sched::Deleted;
                src.work = None;
                src.moved_to = Some(target.as_str().to_owned());
                copy.sched = Sched::Added;
                copy.rev = 0;
                copy.base = None;
                copy.lock = None;
                copy.moved_from = Some(p.as_str().to_owned());
                copy.moved_to = None;
                wc.nodes.insert(target, copy);
            }
            Ok(())
        })
    }

    pub(super) fn do_revert(&self, root: &Path, req: &RevertRequest) -> Result<(), SvnError> {
        self.with_wc(root, |wc| {
            let targets = wc.expand_selection(&req.paths)?;
            let remove_unversioned = req.remove_unversioned.unwrap_or(false);
            for p in targets {
                let Some(n) = wc.nodes.get(&p) else { continue };
                match n.sched {
                    Sched::Unversioned => {
                        if remove_unversioned {
                            wc.nodes.remove(&p);
                        }
                    }
                    Sched::Added => {
                        // svn keeps an added file on disk but forgets it.
                        let n = wc.node_mut(&p)?;
                        n.sched = Sched::Unversioned;
                        n.moved_from = None;
                    }
                    Sched::Normal | Sched::Deleted => {
                        let n = wc.node_mut(&p)?;
                        n.sched = Sched::Normal;
                        n.work = n.base.clone();
                        n.props = n.base_props.clone();
                        n.conflicted = false;
                        n.tree_conflict = false;
                        n.moved_from = None;
                        n.moved_to = None;
                    }
                }
            }
            Ok(())
        })
    }

    pub(super) fn do_cleanup(&self, root: &Path, req: &CleanupRequest) -> Result<(), SvnError> {
        self.with_wc(root, |wc| {
            let drop_unversioned = req.remove_unversioned.unwrap_or(false);
            let drop_ignored = req.remove_ignored.unwrap_or(false);
            let doomed: Vec<RelPath> = wc
                .nodes
                .iter()
                .filter(|(p, n)| {
                    n.sched == Sched::Unversioned
                        && if wc.ignored(p) {
                            drop_ignored
                        } else {
                            drop_unversioned
                        }
                })
                .map(|(p, _)| p.clone())
                .collect();
            wc.nodes.retain(|p, _| !doomed.contains(p));
            Ok(())
        })
    }

    pub(super) fn do_set_depth(
        &self,
        root: &Path,
        path: &str,
        depth: Depth,
    ) -> Result<(), SvnError> {
        let dir = rel(path)?;
        self.with_wc(root, |wc| {
            if wc.node(&dir)?.kind != FileKind::Dir {
                return Err(bad_arg(format!("{dir} is not a directory")));
            }
            let level = |p: &RelPath| p.components().count() - dir.components().count();
            let drop: Vec<RelPath> = wc
                .subtree(&dir)
                .into_iter()
                .filter(|p| *p != dir)
                .filter(|p| match depth {
                    Depth::Empty => true,
                    Depth::Files => level(p) > 1 || wc.nodes[p].kind == FileKind::Dir,
                    Depth::Immediates => level(p) > 1,
                    Depth::Infinity => false,
                })
                .collect();
            if drop.iter().any(|p| {
                let n = &wc.nodes[p];
                n.sched != Sched::Normal || n.work != n.base || n.conflicted
            }) {
                return Err(SvnError::new(
                    ErrorCode::Internal,
                    "cannot reduce depth: local changes below",
                ));
            }
            for p in drop {
                wc.nodes.remove(&p);
            }
            Ok(())
        })
    }

    pub(super) fn do_upgrade(&self, root: &Path, to: u8) -> Result<(), SvnError> {
        let target = opensvn_ipc::WcFormat::try_from(to).map_err(bad_arg)?;
        self.with_wc(root, |wc| {
            wc.format = target;
            Ok(())
        })
    }
}

/// Test and demo hooks that change the in-memory "disk".
impl FakeEngine {
    /// Overwrites a file in the working tree (creates an unversioned file when it is new).
    pub fn edit_file(&self, root: &Path, path: &str, content: &[u8]) -> Result<(), SvnError> {
        let rp = rel(path)?;
        self.with_wc(root, |wc| {
            match wc.nodes.get_mut(&rp) {
                Some(n) if n.kind == FileKind::File => n.work = Some(content.to_vec().into()),
                Some(_) => return Err(bad_arg(format!("{rp} is a directory"))),
                None => {
                    wc.node(&rp.parent().unwrap_or_else(RelPath::root))?;
                    wc.nodes.insert(rp, FakeNode::unversioned_file(content));
                }
            }
            Ok(())
        })
    }

    /// Removes a file from the working tree without telling the working copy.
    pub fn remove_from_disk(&self, root: &Path, path: &str) -> Result<(), SvnError> {
        let rp = rel(path)?;
        self.with_wc(root, |wc| {
            if wc.node(&rp)?.is_versioned() {
                wc.node_mut(&rp)?.work = None;
            } else {
                wc.nodes.remove(&rp);
            }
            Ok(())
        })
    }
}

/// Hooks that simulate lock situations found when a working copy is opened.
impl FakeEngine {
    /// Pretends a crashed process left a lock on `dir`; the next `open_wc` recovers it.
    pub fn simulate_dead_owner_lock(&self, root: &Path, dir: &str) -> Result<(), SvnError> {
        self.with_wc(root, |wc| {
            wc.dead_owner_locks.push(dir.to_owned());
            Ok(())
        })
    }

    /// Pretends another program holds a lock on `dir`; `open_wc` reports it and leaves it alone.
    pub fn simulate_foreign_lock(&self, root: &Path, dir: &str) -> Result<(), SvnError> {
        self.with_wc(root, |wc| {
            wc.foreign_locks.push(dir.to_owned());
            Ok(())
        })
    }

    /// Pretends the working copy folder at `from` was moved to `to` (`to = None`: deleted). The
    /// working copy at `from` can no longer be read, like a folder moved in the file manager.
    pub fn simulate_moved_wc(&self, from: &Path, to: Option<&Path>) -> Result<(), SvnError> {
        let mut wcs = self
            .wcs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut wc = wcs.remove(from).ok_or_else(|| {
            SvnError::new(
                ErrorCode::NotAWorkingCopy,
                format!("{} is not a working copy", from.display()),
            )
        })?;
        if let Some(to) = to {
            wc.root = to.to_path_buf();
            wcs.insert(to.to_path_buf(), wc);
        }
        Ok(())
    }

    /// Pretends the working copy at `root` belongs to another repository (`uuid`) or was relocated
    /// to another repository root URL.
    pub fn simulate_repository(
        &self,
        root: &Path,
        uuid: Option<&str>,
        repo_root_url: Option<&str>,
    ) -> Result<(), SvnError> {
        self.with_wc(root, |wc| {
            if let Some(u) = uuid {
                wc.uuid = u.to_owned();
            }
            if let Some(r) = repo_root_url {
                wc.repo_root_url = r.to_owned();
            }
            Ok(())
        })
    }
}

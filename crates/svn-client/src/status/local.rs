//! Local status: wc.db rows compared with what is on disk.

use std::collections::BTreeSet;
use std::fs;

use svn_diff::diff_as_binary_by_mime;
use svn_types::{
    CancelToken, Depth, ErrorCode, LockInfo, NodeKind, NodeStatus, PropStatus, RelPath,
    RepoDisplayPath, Result, Timestamp, WcStatus,
};
use svn_wc::conflicts::ConflictSkel;
use svn_wc::db::{Node, Presence, WcDb};

use super::StatusOptions;
use super::externals::ExternalPaths;
use super::ignore::{DEFAULT_GLOBAL_IGNORES, is_ignored, split_global, split_property};
use crate::wc::{abs_path, inspect, join_rel};

/// Kind of the item on disk.
fn disk_kind(db: &WcDb, rel: &RelPath) -> Result<(Option<NodeKind>, Option<u64>)> {
    match fs::symlink_metadata(abs_path(db, rel)?) {
        Ok(m) if m.file_type().is_dir() => Ok((Some(NodeKind::Dir), None)),
        Ok(m) if m.file_type().is_symlink() => Ok((Some(NodeKind::Symlink), Some(m.len()))),
        Ok(m) => Ok((Some(NodeKind::File), Some(m.len()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((None, None)),
        Err(e) => Err(svn_types::SvnError::from_io(
            format!("cannot stat '{rel}'"),
            &e,
        )),
    }
}

fn lock_of(db: &WcDb, rel: &RelPath, repos_path: &str) -> Result<Option<LockInfo>> {
    Ok(db.repos_lock(rel)?.map(|l| LockInfo {
        path: RepoDisplayPath::parse(&format!("/{repos_path}"))
            .unwrap_or_else(|_| RepoDisplayPath::from_rel(rel)),
        token: l.token,
        owner: l.owner.unwrap_or_default(),
        comment: l.comment,
        created: Timestamp(l.date.unwrap_or(0)),
        expires: None,
    }))
}

fn moved_from(db: &WcDb, rel: &RelPath) -> Result<Option<RelPath>> {
    db.moved_from(rel)
}

/// Status of one versioned node, `None` for nodes that are not part of the working copy.
fn node_status(db: &WcDb, rel: &RelPath, cur: &Node) -> Result<Option<WcStatus>> {
    let base = db.read_base(rel)?;
    let (on_disk, size) = disk_kind(db, rel)?;
    let mut text = match cur.presence {
        Presence::NotPresent | Presence::ServerExcluded => return Ok(None),
        Presence::Excluded => NodeStatus::Excluded,
        Presence::BaseDeleted => NodeStatus::Deleted,
        Presence::Incomplete => NodeStatus::Incomplete,
        Presence::Normal if cur.op_depth > 0 => {
            if base
                .as_ref()
                .is_some_and(|b| b.presence == Presence::Normal)
            {
                NodeStatus::Replaced
            } else {
                NodeStatus::Added
            }
        }
        Presence::Normal => NodeStatus::Normal,
    };
    let versioned_here = matches!(cur.presence, Presence::Normal | Presence::Incomplete);
    if versioned_here {
        match (on_disk, cur.kind) {
            (None, _) => text = NodeStatus::Missing,
            (Some(k), want)
                if k != want
                    && !(want == NodeKind::File && k == NodeKind::Symlink)
                    && want != NodeKind::None =>
            {
                text = NodeStatus::Obstructed;
            }
            (Some(NodeKind::File | NodeKind::Symlink), NodeKind::File | NodeKind::Symlink)
                if text == NodeStatus::Normal && inspect::text_modified(db, rel, cur)? =>
            {
                text = NodeStatus::Modified;
            }
            _ => {}
        }
    }
    let actual = db.actual_props(rel)?;
    let mut props = if inspect::props_modified(db, rel)? {
        PropStatus::Modified
    } else if db.pristine_props(rel)?.is_empty() && actual.is_empty() {
        PropStatus::None
    } else {
        PropStatus::Normal
    };
    let mut tree_conflict = false;
    if let Some(rec) = db.conflict(rel)? {
        let skel = ConflictSkel::from_skel(&rec.skel)?;
        if skel.text().is_some() {
            text = NodeStatus::Conflicted;
        }
        if skel.prop().is_some() {
            props = PropStatus::Conflicted;
        }
        tree_conflict = skel.tree().is_some();
    }
    let repos_path = cur
        .repos_relpath
        .clone()
        .or_else(|| base.as_ref().and_then(|b| b.repos_relpath.clone()))
        .unwrap_or_default();
    let parent_repos = match rel.parent() {
        Some(p) => db.read_base(&p)?.and_then(|n| n.repos_relpath),
        None => None,
    };
    // File externals point anywhere by definition and are reported as such, not as switched.
    let switched = cur.op_depth == 0
        && !cur.file_external
        && parent_repos.is_some_and(|p| {
            let name = rel.file_name().unwrap_or("");
            let expected = if p.is_empty() {
                name.to_owned()
            } else {
                format!("{p}/{name}")
            };
            expected != repos_path
        });
    let lock = if cur.op_depth == 0 {
        lock_of(db, rel, &repos_path)?
    } else {
        None
    };
    let mime =
        inspect::prop_value(&actual, "svn:mime-type").and_then(|v| std::str::from_utf8(v).ok());
    Ok(Some(WcStatus {
        path: rel.clone(),
        kind: cur.kind,
        text,
        props,
        tree_conflict,
        moved_from: if cur.moved_here {
            moved_from(db, rel)?
        } else {
            None
        },
        moved_to: cur.moved_to.clone(),
        changelist: db.changelist(rel)?,
        lock_is_mine: lock.is_some(),
        lock,
        needs_lock: inspect::prop_value(&actual, "svn:needs-lock").is_some(),
        binary: cur.kind == NodeKind::File && diff_as_binary_by_mime(mime),
        size: if cur.kind == NodeKind::File {
            size
        } else {
            None
        },
        remote: None,
        switched,
        file_external: cur.file_external,
        depth: cur.depth.unwrap_or(Depth::Unknown),
        base_rev: base
            .as_ref()
            .and_then(|b| b.revision)
            .or(cur.revision)
            .map(svn_types::Revnum),
    }))
}

/// True when the plain listing would show the entry (svn's non-verbose rule).
fn is_interesting(s: &WcStatus) -> bool {
    s.text != NodeStatus::Normal
        || !matches!(s.props, PropStatus::None | PropStatus::Normal)
        || s.tree_conflict
        || s.switched
        || s.file_external
        || s.lock.is_some()
        || s.changelist.is_some()
}

struct Walker<'a> {
    db: &'a WcDb,
    opts: &'a StatusOptions,
    cancel: &'a CancelToken,
    global: Vec<String>,
    externals: ExternalPaths,
    out: Vec<WcStatus>,
}

impl Walker<'_> {
    /// Reports an unversioned item as `text` (`Unversioned`, `Ignored` or `External`).
    fn unversioned(&mut self, rel: &RelPath, text: NodeStatus) -> Result<()> {
        let (kind, size) = disk_kind(self.db, rel)?;
        let wanted = if text == NodeStatus::Ignored {
            self.opts.include_ignored
        } else {
            self.opts.include_unversioned
        };
        if !wanted {
            return Ok(());
        }
        self.out.push(WcStatus {
            path: rel.clone(),
            kind: kind.unwrap_or(NodeKind::Unknown),
            text,
            props: PropStatus::None,
            tree_conflict: false,
            moved_from: None,
            moved_to: None,
            changelist: None,
            lock: None,
            lock_is_mine: false,
            needs_lock: false,
            binary: false,
            size,
            remote: None,
            switched: false,
            file_external: false,
            depth: Depth::Unknown,
            base_rev: None,
        });
        Ok(())
    }

    /// The child is a directory: by its working node when versioned, else on disk.
    fn child_is_dir(&self, child: &RelPath, versioned: bool) -> Result<bool> {
        if versioned && let Some(n) = self.db.read_node(child)? {
            return Ok(n.kind == NodeKind::Dir);
        }
        Ok(fs::symlink_metadata(abs_path(self.db, child)?).is_ok_and(|m| m.is_dir()))
    }

    fn walk(&mut self, rel: &RelPath, depth: Depth) -> Result<()> {
        self.cancel.check()?;
        let Some(cur) = self.db.read_node(rel)? else {
            return Ok(());
        };
        let status = node_status(self.db, rel, &cur)?;
        if let Some(s) = status
            && (self.opts.show_unmodified || is_interesting(&s))
        {
            self.out.push(s);
        }
        if cur.kind != NodeKind::Dir
            || !matches!(cur.presence, Presence::Normal | Presence::Incomplete)
            || depth == Depth::Empty
        {
            return Ok(());
        }
        // Externals defined here may live anywhere below.
        self.externals.add_defined_in(self.db, rel)?;
        // Names of versioned children and of whatever is on disk.
        let mut names: BTreeSet<String> = BTreeSet::new();
        for c in self.db.children(rel)? {
            if let Some(n) = c.file_name() {
                names.insert(n.to_owned());
            }
        }
        let versioned: BTreeSet<String> = names.clone();
        let dir_path = abs_path(self.db, rel)?;
        if let Ok(rd) = fs::read_dir(&dir_path) {
            for e in rd.flatten() {
                if let Some(n) = e.file_name().to_str()
                    && n != svn_wc::db::ADM_DIR
                {
                    names.insert(n.to_owned());
                }
            }
        }
        let mut local_ignores: Option<Vec<String>> = None;
        for name in names {
            let child = join_rel(rel, &RelPath::new(&name)?)?;
            if depth == Depth::Files && self.child_is_dir(&child, versioned.contains(&name))? {
                // `--depth files`: subdirectories are not part of the status.
                continue;
            }
            if versioned.contains(&name) {
                // Below `infinity` the children are listed but not descended into.
                let next = if depth == Depth::Infinity {
                    Depth::Infinity
                } else {
                    Depth::Empty
                };
                self.walk(&child, next)?;
            } else if self.externals.covers(&child) {
                // An external's directory (or one leading to it) is reported as such, never as
                // ignored, and not descended into (`svn status --ignore-externals`).
                self.unversioned(&child, NodeStatus::External)?;
            } else if self.opts.include_unversioned || self.opts.include_ignored {
                let ignores = local_ignores.get_or_insert_with(|| {
                    let mut p = self.global.clone();
                    if let Ok(props) = self.db.actual_props(rel)
                        && let Some(v) = inspect::prop_value(&props, "svn:ignore")
                    {
                        p.extend(split_property(v));
                    }
                    p
                });
                let text = if is_ignored(ignores, &name) {
                    NodeStatus::Ignored
                } else {
                    NodeStatus::Unversioned
                };
                self.unversioned(&child, text)?;
            }
        }
        Ok(())
    }
}

/// Local status of `rel` and, depending on `opts.depth`, what is below it.
///
/// Externals are reported like `svn status --ignore-externals`: the directory of a directory
/// external (and any unversioned directory leading to one) is one [`NodeStatus::External`] item
/// and is not descended into; file externals are versioned items with `file_external` set.
/// A separate working copy checked out inside this one is reported as an unversioned directory,
/// as svn does; [`is_wc_root_dir`](super::is_wc_root_dir) tells callers that must not treat it as
/// disposable.
pub fn local_status(
    db: &WcDb,
    rel: &RelPath,
    opts: &StatusOptions,
    global_ignores: Option<&[String]>,
    cancel: &CancelToken,
) -> Result<Vec<WcStatus>> {
    if db.read_node(rel)?.is_none() {
        return Err(svn_types::SvnError::new(
            ErrorCode::PathNotFound,
            format!("'{rel}' is not under version control"),
        ));
    }
    let global = global_ignores
        .map(<[String]>::to_vec)
        .unwrap_or_else(|| split_global(DEFAULT_GLOBAL_IGNORES));
    let mut w = Walker {
        db,
        opts,
        cancel,
        global,
        externals: ExternalPaths::for_path(db, rel)?,
        out: Vec::new(),
    };
    let depth = if opts.depth == Depth::Unknown {
        Depth::Infinity
    } else {
        opts.depth
    };
    w.walk(rel, depth)?;
    Ok(w.out)
}

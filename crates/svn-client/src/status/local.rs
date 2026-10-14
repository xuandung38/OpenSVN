//! Local status: wc.db rows compared with what is on disk.
//!
//! The walk reads the database once ([`WcDb::read_subtree`]: every NODES and ACTUAL_NODE row
//! of the target plus the lock cache, in one read transaction) and `lstat`s every entry of a
//! directory once while listing it. Files whose size and mtime match the values recorded in
//! wc.db are unmodified without being read (svn's rule); the others are compared with their
//! pristine text in repository-normal form. The decisions per node are the same as reading
//! each node with [`WcDb::read_node`] and friends.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use svn_diff::diff_as_binary_by_mime;
use svn_types::{
    CancelToken, Depth, ErrorCode, LockInfo, NodeKind, NodeStatus, PropStatus, RelPath,
    RepoDisplayPath, Result, Timestamp, WcStatus, safe_join_child,
};
use svn_wc::conflicts::ConflictSkel;
use svn_wc::db::{ActualRow, ConflictRecord, Node, Presence, Props, SubtreeRows, WcDb};

use super::StatusOptions;
use super::externals::ExternalPaths;
use super::ignore::{DEFAULT_GLOBAL_IGNORES, is_ignored, split_global, split_property};
use crate::wc::{abs_path, inspect, join_rel};

/// What `lstat` found at a path: `None` when nothing is there.
type Disk = Option<fs::Metadata>;

/// `lstat` of `rel`; a missing item is `None`, other failures are errors.
fn lstat(abs: &Path, rel: &RelPath) -> Result<Disk> {
    match fs::symlink_metadata(abs) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(svn_types::SvnError::from_io(
            format!("cannot stat '{rel}'"),
            &e,
        )),
    }
}

/// Kind of the item on disk and its size (files and symlinks).
fn disk_kind(disk: Option<&fs::Metadata>) -> (Option<NodeKind>, Option<u64>) {
    match disk {
        None => (None, None),
        Some(m) if m.file_type().is_dir() => (Some(NodeKind::Dir), None),
        Some(m) if m.file_type().is_symlink() => (Some(NodeKind::Symlink), Some(m.len())),
        Some(m) => (Some(NodeKind::File), Some(m.len())),
    }
}

/// The rows of the walked subtree, read once, with the single-node readers of [`WcDb`] as the
/// fallback for paths outside it (the target's ancestors).
struct Snapshot<'a> {
    db: &'a WcDb,
    root: RelPath,
    /// Rows of each path, lowest `op_depth` first.
    nodes: HashMap<&'a str, &'a [Node]>,
    /// Distinct child paths of each path that has rows, sorted.
    children: HashMap<&'a str, Vec<&'a RelPath>>,
    actual: HashMap<&'a str, &'a ActualRow>,
    /// Lock cache by repository location.
    locks: HashMap<(i64, &'a str), &'a svn_wc::db::LockInfo>,
}

impl<'a> Snapshot<'a> {
    /// Indexes `rows`, the [`WcDb::read_subtree`] of `root`.
    fn new(db: &'a WcDb, root: &RelPath, rows: &'a SubtreeRows) -> Self {
        let mut nodes: HashMap<&'a str, &'a [Node]> = HashMap::with_capacity(rows.nodes.len());
        let mut children: HashMap<&'a str, Vec<&'a RelPath>> = HashMap::new();
        // Rows come ordered by path then `op_depth`: each run of equal paths is one node, and
        // each path is listed under its parent once and in order.
        let mut start = 0;
        while start < rows.nodes.len() {
            let path = &rows.nodes[start].relpath;
            let end = rows.nodes[start..]
                .iter()
                .position(|n| n.relpath != *path)
                .map_or(rows.nodes.len(), |len| start + len);
            if path != root && !path.is_root() {
                let parent = path.as_str().rsplit_once('/').map_or("", |(p, _)| p);
                children.entry(parent).or_default().push(path);
            }
            nodes.insert(path.as_str(), &rows.nodes[start..end]);
            start = end;
        }
        Self {
            db,
            root: root.clone(),
            nodes,
            children,
            actual: rows
                .actual
                .iter()
                .map(|a| (a.relpath.as_str(), a))
                .collect(),
            locks: rows
                .locks
                .iter()
                .map(|l| ((l.repos_id, l.repos_relpath.as_str()), &l.lock))
                .collect(),
        }
    }

    fn inside(&self, rel: &RelPath) -> bool {
        rel.starts_with(&self.root)
    }

    /// [`WcDb::read_node`].
    fn read_node(&self, rel: &RelPath) -> Result<Option<Cow<'_, Node>>> {
        if self.inside(rel) {
            return Ok(self
                .nodes
                .get(rel.as_str())
                .and_then(|v| v.last())
                .map(Cow::Borrowed));
        }
        Ok(self.db.read_node(rel)?.map(Cow::Owned))
    }

    /// [`WcDb::read_base`].
    fn read_base(&self, rel: &RelPath) -> Result<Option<Cow<'_, Node>>> {
        if self.inside(rel) {
            return Ok(self
                .nodes
                .get(rel.as_str())
                .and_then(|v| v.first())
                .filter(|n| n.op_depth == 0)
                .map(Cow::Borrowed));
        }
        Ok(self.db.read_base(rel)?.map(Cow::Owned))
    }

    /// [`WcDb::children`] of a path inside the subtree.
    fn children(&self, rel: &RelPath) -> &[&'a RelPath] {
        self.children.get(rel.as_str()).map_or(&[], Vec::as_slice)
    }

    /// [`WcDb::pristine_props`].
    fn pristine_props(&self, rel: &RelPath) -> Result<Cow<'_, Props>> {
        if self.inside(rel)
            && let Some(cur) = self.nodes.get(rel.as_str()).and_then(|v| v.last())
        {
            return Ok(match (&cur.properties, cur.presence.is_present()) {
                (Some(p), true) => Cow::Borrowed(p),
                _ => Cow::Owned(Vec::new()),
            });
        }
        // Outside the subtree, or no node (the reader reports that error).
        Ok(Cow::Owned(self.db.pristine_props(rel)?))
    }

    /// Locally changed properties recorded in ACTUAL_NODE, if any.
    fn changed_props(&self, rel: &RelPath) -> Result<Option<Cow<'_, Props>>> {
        if self.inside(rel) {
            return Ok(self
                .actual
                .get(rel.as_str())
                .and_then(|a| a.properties.as_ref())
                .map(Cow::Borrowed));
        }
        let actual = self.db.actual_props(rel)?;
        let pristine = self.db.pristine_props(rel)?;
        Ok((actual != pristine).then_some(Cow::Owned(actual)))
    }

    /// [`WcDb::actual_props`].
    fn actual_props(&self, rel: &RelPath) -> Result<Cow<'_, Props>> {
        match self.changed_props(rel)? {
            Some(p) => Ok(p),
            None => self.pristine_props(rel),
        }
    }

    /// [`WcDb::conflict`] of a path inside the subtree.
    fn conflict(&self, rel: &RelPath) -> Option<&ConflictRecord> {
        self.actual
            .get(rel.as_str())
            .and_then(|a| a.conflict.as_ref())
    }

    /// [`WcDb::changelist`] of a path inside the subtree.
    fn changelist(&self, rel: &RelPath) -> Option<String> {
        self.actual
            .get(rel.as_str())
            .and_then(|a| a.changelist.clone())
    }

    /// [`WcDb::repos_lock`]: the cached lock matching the repository location of the BASE row.
    fn repos_lock(&self, rel: &RelPath) -> Result<Option<svn_wc::db::LockInfo>> {
        let Some(base) = self.read_base(rel)? else {
            return Ok(None);
        };
        let (Some(id), Some(path)) = (base.repos_id, base.repos_relpath.as_deref()) else {
            return Ok(None);
        };
        Ok(self.locks.get(&(id, path)).map(|l| (*l).clone()))
    }
}

fn lock_of(snap: &Snapshot<'_>, rel: &RelPath, repos_path: &str) -> Result<Option<LockInfo>> {
    Ok(snap.repos_lock(rel)?.map(|l| LockInfo {
        path: RepoDisplayPath::parse(&format!("/{repos_path}"))
            .unwrap_or_else(|_| RepoDisplayPath::from_rel(rel)),
        token: l.token,
        owner: l.owner.unwrap_or_default(),
        comment: l.comment,
        created: Timestamp(l.date.unwrap_or(0)),
        expires: None,
    }))
}

fn sorted(p: &Props) -> Props {
    let mut v = p.clone();
    v.sort();
    v
}

/// Status of one versioned node, `None` for nodes that are not part of the working copy.
/// `disk` is the `lstat` of its working item.
fn node_status(
    snap: &Snapshot<'_>,
    rel: &RelPath,
    abs: &Path,
    cur: &Node,
    disk: Option<&fs::Metadata>,
) -> Result<Option<WcStatus>> {
    let db = snap.db;
    let base = snap.read_base(rel)?;
    let (on_disk, size) = disk_kind(disk);
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
        match (on_disk, cur.kind, disk) {
            (None, _, _) => text = NodeStatus::Missing,
            (Some(k), want, _)
                if k != want
                    && !(want == NodeKind::File && k == NodeKind::Symlink)
                    && want != NodeKind::None =>
            {
                text = NodeStatus::Obstructed;
            }
            (
                Some(NodeKind::File | NodeKind::Symlink),
                NodeKind::File | NodeKind::Symlink,
                Some(md),
            ) if text == NodeStatus::Normal
                && inspect::text_modified_stat(abs, md, cur, || {
                    snap.actual_props(rel).map(Cow::into_owned)
                })? =>
            {
                text = NodeStatus::Modified;
            }
            _ => {}
        }
    }
    let changed = snap.changed_props(rel)?;
    let pristine = snap.pristine_props(rel)?;
    let actual: &Props = changed.as_deref().unwrap_or(&pristine);
    // `inspect::props_modified`: the working set differs from the pristine one.
    let mut props = if changed
        .as_deref()
        .is_some_and(|c| sorted(c) != sorted(&pristine))
    {
        PropStatus::Modified
    } else if pristine.is_empty() && actual.is_empty() {
        PropStatus::None
    } else {
        PropStatus::Normal
    };
    let mut tree_conflict = false;
    let conflict = if snap.inside(rel) {
        snap.conflict(rel).cloned()
    } else {
        db.conflict(rel)?
    };
    if let Some(rec) = conflict {
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
        Some(p) => snap.read_base(&p)?.and_then(|n| n.repos_relpath.clone()),
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
        lock_of(snap, rel, &repos_path)?
    } else {
        None
    };
    let mime =
        inspect::prop_value(actual, "svn:mime-type").and_then(|v| std::str::from_utf8(v).ok());
    Ok(Some(WcStatus {
        path: rel.clone(),
        kind: cur.kind,
        text,
        props,
        tree_conflict,
        moved_from: if cur.moved_here {
            db.moved_from(rel)?
        } else {
            None
        },
        moved_to: cur.moved_to.clone(),
        changelist: if snap.inside(rel) {
            snap.changelist(rel)
        } else {
            db.changelist(rel)?
        },
        lock_is_mine: lock.is_some(),
        lock,
        needs_lock: inspect::prop_value(actual, "svn:needs-lock").is_some(),
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

/// The entries of one directory listing with their `lstat` (`None` when it failed, so the
/// item is looked up again by path).
type Listing = HashMap<String, Option<fs::Metadata>>;

/// `lstat` of the child `name` of a listed directory: from the listing when it is there, else by
/// path (a versioned item the listing did not show is usually missing).
fn child_disk(listing: &mut Listing, name: &str, child: &RelPath, abs: &Path) -> Result<Disk> {
    match listing.remove(name) {
        Some(Some(md)) => Ok(Some(md)),
        _ => lstat(abs, child),
    }
}

/// Status entry of an unversioned item reported as `text` (`Unversioned`, `Ignored` or
/// `External`).
fn unversioned_status(
    rel: RelPath,
    text: NodeStatus,
    kind: Option<NodeKind>,
    size: Option<u64>,
) -> WcStatus {
    WcStatus {
        path: rel,
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
    }
}

/// The walk descends into `node` (reached with `depth`): a present directory and a depth that
/// lists children.
fn descends(node: &Node, depth: Depth) -> bool {
    node.kind == NodeKind::Dir
        && matches!(node.presence, Presence::Normal | Presence::Incomplete)
        && depth != Depth::Empty
}

/// Depth a versioned child is walked with: below `infinity` the children are listed but not
/// descended into.
fn child_depth(depth: Depth) -> Depth {
    if depth == Depth::Infinity {
        Depth::Infinity
    } else {
        Depth::Empty
    }
}

/// One directory whose children the walk lists.
struct DirJob {
    rel: RelPath,
    depth: Depth,
    /// Path and `lstat` of the walk's target (the other directories look theirs up).
    target: Option<(std::path::PathBuf, Disk)>,
}

/// What listing one directory produced, in the walk's order.
enum Item {
    /// A versioned child to report.
    Status(WcStatus),
    /// A versioned child directory the walk descends into: index into the jobs.
    Dir(usize),
    /// An unversioned child; whether it is an external is decided in walk order, since the
    /// definitions seen so far decide it (like svn's `is_external_path`). `text` is
    /// `Unversioned` or `Ignored`, `None` when such items are not reported.
    Unversioned {
        rel: RelPath,
        kind: Option<NodeKind>,
        size: Option<u64>,
        text: Option<NodeStatus>,
    },
    /// The walk stops here with this error.
    Err(svn_types::SvnError),
}

/// Lists the children of the job directories, possibly on several threads. The decisions are
/// those of a sequential depth-first walk; [`assemble`] replays the items in that order, so the
/// output and the first error are the same as walking on one thread.
struct Lister<'a> {
    snap: &'a Snapshot<'a>,
    opts: &'a StatusOptions,
    cancel: &'a CancelToken,
    global: &'a [String],
    /// Job index of each directory the walk descends into.
    index: HashMap<&'a str, usize>,
}

impl Lister<'_> {
    fn list(&self, job: &DirJob) -> Vec<Item> {
        let mut items = Vec::new();
        if let Err(e) = self.list_into(job, &mut items) {
            items.push(Item::Err(e));
        }
        items
    }

    fn list_into(&self, job: &DirJob, items: &mut Vec<Item>) -> Result<()> {
        let (rel, depth, snap) = (&job.rel, job.depth, self.snap);
        let (abs, disk) = match &job.target {
            Some((abs, disk)) => (abs.clone(), disk.clone()),
            None => {
                // The parent's listing proved the same path safe before this job's items count.
                let abs = abs_path(snap.db, rel)?;
                let disk = lstat(&abs, rel)?;
                (abs, disk)
            }
        };
        // Names of versioned children and of whatever is on disk, with one `lstat` per entry.
        let mut names: BTreeSet<String> = BTreeSet::new();
        for c in snap.children(rel) {
            if let Some(n) = c.file_name() {
                names.insert(n.to_owned());
            }
        }
        let versioned: BTreeSet<String> = names.clone();
        let mut listing = Listing::new();
        if let Ok(rd) = fs::read_dir(&abs) {
            for e in rd.flatten() {
                if let Some(n) = e.file_name().to_str()
                    && n != svn_wc::db::ADM_DIR
                {
                    names.insert(n.to_owned());
                    // `DirEntry::metadata` does not follow symlinks, like `lstat`.
                    listing.insert(n.to_owned(), e.metadata().ok());
                }
            }
        }
        let mut local_ignores: Option<Vec<String>> = None;
        for name in names {
            let child = join_rel(rel, &RelPath::new(&name)?)?;
            // Same path and errors as `abs_path(child)`, without `lstat`ing the ancestors again.
            let child_abs = safe_join_child(&abs, disk.as_ref(), &child)?;
            let is_versioned = versioned.contains(&name);
            let child_disk = child_disk(&mut listing, &name, &child, &child_abs)?;
            if depth == Depth::Files {
                // `--depth files`: subdirectories are not part of the status. A versioned child
                // is a directory by its working node, anything else by what is on disk.
                let node = if is_versioned {
                    snap.read_node(&child)?
                } else {
                    None
                };
                let is_dir = match node {
                    Some(n) => n.kind == NodeKind::Dir,
                    None => child_disk.as_ref().is_some_and(fs::Metadata::is_dir),
                };
                if is_dir {
                    continue;
                }
            }
            if is_versioned {
                self.cancel.check()?;
                let Some(cur) = snap.read_node(&child)? else {
                    continue;
                };
                if let Some(s) = node_status(snap, &child, &child_abs, &cur, child_disk.as_ref())?
                    && (self.opts.show_unmodified || is_interesting(&s))
                {
                    items.push(Item::Status(s));
                }
                if descends(&cur, child_depth(depth)) {
                    let Some(&j) = self.index.get(child.as_str()) else {
                        return Err(svn_types::SvnError::new(
                            ErrorCode::Internal,
                            format!("status walk lost directory '{child}'"),
                        ));
                    };
                    items.push(Item::Dir(j));
                }
            } else {
                let text = if self.opts.include_unversioned || self.opts.include_ignored {
                    let ignores = local_ignores.get_or_insert_with(|| {
                        let mut p = self.global.to_vec();
                        if let Ok(props) = snap.actual_props(rel)
                            && let Some(v) = inspect::prop_value(&props, "svn:ignore")
                        {
                            p.extend(split_property(v));
                        }
                        p
                    });
                    Some(if is_ignored(ignores, &name) {
                        NodeStatus::Ignored
                    } else {
                        NodeStatus::Unversioned
                    })
                } else {
                    None
                };
                let (kind, size) = disk_kind(child_disk.as_ref());
                items.push(Item::Unversioned {
                    rel: child,
                    kind,
                    size,
                    text,
                });
            }
        }
        Ok(())
    }
}

/// The directories a walk of `target` (with `depth`) descends into, parents before children.
fn dir_jobs(snap: &Snapshot<'_>, target: &RelPath, depth: Depth) -> Result<Vec<(RelPath, Depth)>> {
    let mut out = Vec::new();
    let Some(cur) = snap.read_node(target)? else {
        return Ok(out);
    };
    if !descends(&cur, depth) {
        return Ok(out);
    }
    out.push((target.clone(), depth));
    let mut i = 0;
    while i < out.len() {
        let (dir, d) = out[i].clone();
        i += 1;
        // `--depth files` skips child directories; other depths walk them with `child_depth`.
        if d == Depth::Files {
            continue;
        }
        for c in snap.children(&dir) {
            if let Some(n) = snap.read_node(c)?
                && descends(&n, child_depth(d))
            {
                out.push(((*c).clone(), child_depth(d)));
            }
        }
    }
    Ok(out)
}

/// Lists every job directory, on up to `available_parallelism` threads.
fn list_all(lister: &Lister<'_>, jobs: &[DirJob]) -> Vec<Vec<Item>> {
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(jobs.len());
    if workers <= 1 {
        return jobs.iter().map(|j| lister.list(j)).collect();
    }
    let next = AtomicUsize::new(0);
    let mut results: Vec<Vec<Item>> = jobs.iter().map(|_| Vec::new()).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let next = &next;
                scope.spawn(move || {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(job) = jobs.get(i) else { break };
                        done.push((i, lister.list(job)));
                    }
                    done
                })
            })
            .collect();
        for h in handles {
            match h.join() {
                Ok(done) => {
                    for (i, items) in done {
                        results[i] = items;
                    }
                }
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
    });
    results
}

/// Replays the listed items depth-first from job `start`, deciding externals in walk order, and
/// stops at the first error.
fn assemble(
    snap: &Snapshot<'_>,
    jobs: &[DirJob],
    mut listed: Vec<Vec<Item>>,
    start: usize,
    opts: &StatusOptions,
    externals: &mut ExternalPaths,
    out: &mut Vec<WcStatus>,
) -> Result<()> {
    let mut stack: Vec<std::vec::IntoIter<Item>> = Vec::new();
    let enter = |j: usize,
                 listed: &mut Vec<Vec<Item>>,
                 externals: &mut ExternalPaths|
     -> Result<std::vec::IntoIter<Item>> {
        // Externals defined here may live anywhere below.
        let rel = &jobs[j].rel;
        let (pristine, actual) = (snap.pristine_props(rel)?, snap.actual_props(rel)?);
        externals.add_defined_by(rel, &pristine, &actual);
        Ok(std::mem::take(&mut listed[j]).into_iter())
    };
    stack.push(enter(start, &mut listed, externals)?);
    while let Some(top) = stack.last_mut() {
        let Some(item) = top.next() else {
            stack.pop();
            continue;
        };
        match item {
            Item::Status(s) => out.push(s),
            Item::Dir(j) => {
                let it = enter(j, &mut listed, externals)?;
                stack.push(it);
            }
            Item::Unversioned {
                rel,
                kind,
                size,
                text,
            } => {
                // An external's directory (or one leading to it) is reported as such, never as
                // ignored, and not descended into (`svn status --ignore-externals`).
                let text = if externals.covers(&rel) {
                    Some(NodeStatus::External)
                } else {
                    text
                };
                let wanted = match text {
                    Some(NodeStatus::Ignored) => opts.include_ignored,
                    Some(_) => opts.include_unversioned,
                    None => false,
                };
                if let Some(t) = text
                    && wanted
                {
                    out.push(unversioned_status(rel, t, kind, size));
                }
            }
            Item::Err(e) => return Err(e),
        }
    }
    Ok(())
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
    let rows = db.read_subtree(rel)?;
    let snap = Snapshot::new(db, rel, &rows);
    let mut externals = ExternalPaths::for_path(db, rel)?;
    let depth = if opts.depth == Depth::Unknown {
        Depth::Infinity
    } else {
        opts.depth
    };
    let mut out = Vec::new();

    // The target itself.
    cancel.check()?;
    let abs = abs_path(db, rel)?;
    let disk = lstat(&abs, rel)?;
    let Some(cur) = snap.read_node(rel)? else {
        return Ok(out);
    };
    if let Some(s) = node_status(&snap, rel, &abs, &cur, disk.as_ref())?
        && (opts.show_unmodified || is_interesting(&s))
    {
        out.push(s);
    }
    drop(cur);

    // Everything below it: list the directories (in parallel), then replay in walk order.
    let dirs = dir_jobs(&snap, rel, depth)?;
    if dirs.is_empty() {
        return Ok(out);
    }
    let mut target = Some((abs, disk));
    let jobs: Vec<DirJob> = dirs
        .into_iter()
        .map(|(r, d)| DirJob {
            target: if r == *rel { target.take() } else { None },
            rel: r,
            depth: d,
        })
        .collect();
    let lister = Lister {
        snap: &snap,
        opts,
        cancel,
        global: &global,
        index: jobs
            .iter()
            .enumerate()
            .map(|(i, j)| (j.rel.as_str(), i))
            .collect(),
    };
    let listed = list_all(&lister, &jobs);
    assemble(&snap, &jobs, listed, 0, opts, &mut externals, &mut out)?;
    Ok(out)
}

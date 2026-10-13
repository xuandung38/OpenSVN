//! Local status (`svn status` without `-u`): one parallel filesystem scan, one bulk read of the
//! node tables, and svn's own decision logic (`assemble_status`, `one_child_status`,
//! `send_unversioned_item` from `libsvn_wc/status.c`, plus the conflict post-processing of
//! `libsvn_client/status.c`; identical in svn 1.14.5 and 1.15.0).
//!
//! How it is fast: the directory tree is walked by several threads that `lstat` every entry;
//! unchanged files are recognised from the size and mtime recorded in wc.db
//! ([`modified::file_modified`] documents the fast path), so a repeated status reads no file
//! content. Files whose recorded stat does not match are compared with their pristine text in
//! parallel, and the proven-unchanged ones can have their stat cache repaired
//! ([`StatusOptions::update_cache`]; the caller must hold the WC write lock, like svn).
//!
//! Scope: infinite depth below a target, local state only (no repository columns, no externals
//! beyond the `file_external` flag, no lock tokens). Symlink handling is POSIX; on Windows a
//! `svn:special` file is treated as a regular file.

pub mod ignores;
pub mod modified;

mod load;
mod scan;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use rusqlite::params;
use svn_types::{CancelToken, ErrorCode, NodeKind, RelPath, SvnError};

use crate::conflicts::{Conflict, ConflictSkel, TreeAction, TreeReason};
use crate::db::{ADM_DIR, WcDb, sql_err};
use crate::translate::TranslateInfo;
use crate::workqueue::PristineSource;

use load::{ActualRec, DbStatus, NodeRec, Presence};
use modified::{DiskFile, RecordedFile};
use scan::{FsEntry, FsKind};

/// `svn_wc_status_kind`, restricted to the values local status produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemStatus {
    /// Nothing to report (`svn_wc_status_none`).
    None,
    /// Versioned and unchanged.
    Normal,
    /// Text or properties modified.
    Modified,
    /// Scheduled for addition (also copies).
    Added,
    /// Scheduled for deletion.
    Deleted,
    /// Deleted and added again.
    Replaced,
    /// Versioned but absent from disk.
    Missing,
    /// Versioned as a file but a directory (or the reverse) is on disk.
    Obstructed,
    /// On disk, not versioned.
    Unversioned,
    /// On disk, not versioned, matches an ignore pattern.
    Ignored,
    /// Text or property conflict.
    Conflicted,
    /// Update was interrupted for this node.
    Incomplete,
}

impl ItemStatus {
    /// The word `svn status --xml` uses for `item` / `props`.
    pub fn xml_word(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Normal => "normal",
            Self::Modified => "modified",
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Replaced => "replaced",
            Self::Missing => "missing",
            Self::Obstructed => "obstructed",
            Self::Unversioned => "unversioned",
            Self::Ignored => "ignored",
            Self::Conflicted => "conflicted",
            Self::Incomplete => "incomplete",
        }
    }
}

/// One path of the status report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// Path relative to the working copy root.
    pub path: RelPath,
    /// The path is under version control.
    pub versioned: bool,
    /// Versioned node kind (`None` for unversioned paths).
    pub kind: NodeKind,
    /// `node_status` including the conflict promotion done by the svn client library.
    pub node_status: ItemStatus,
    /// `text_status`.
    pub text_status: ItemStatus,
    /// `prop_status` (`None`, `Normal`, `Modified` or `Conflicted`).
    pub prop_status: ItemStatus,
    /// Copied (`+` column).
    pub copied: bool,
    /// Switched to another repository path.
    pub switched: bool,
    /// A file external.
    pub file_external: bool,
    /// BASE revision; `None` when svn would not report one.
    pub revision: Option<u64>,
    /// Last changed revision.
    pub changed_rev: Option<u64>,
    /// Last changed author.
    pub changed_author: Option<String>,
    /// Last changed date, APR microseconds.
    pub changed_date: Option<i64>,
    /// Changelist.
    pub changelist: Option<String>,
    /// A text conflict whose marker file still exists.
    pub text_conflicted: bool,
    /// A property conflict whose reject file still exists.
    pub prop_conflicted: bool,
    /// A tree conflict.
    pub tree_conflicted: bool,
    /// Destination of a move that starts at this path.
    pub moved_to: Option<RelPath>,
}

impl StatusEntry {
    /// The `item` attribute of `svn status --xml` (`combined_status` in `svn/status.c`).
    pub fn xml_item(&self) -> ItemStatus {
        match self.node_status {
            ItemStatus::Conflicted if !self.versioned && self.tree_conflicted => ItemStatus::Missing,
            ItemStatus::Conflicted | ItemStatus::Modified => self.text_status,
            other => other,
        }
    }

    /// The `props` attribute of `svn status --xml`.
    pub fn xml_props(&self) -> ItemStatus {
        if self.node_status == ItemStatus::Deleted { ItemStatus::None } else { self.prop_status }
    }
}

/// Parameters of a status run.
#[derive(Debug, Clone)]
pub struct StatusOptions {
    /// Report unmodified entries as well (`svn status -v`).
    pub get_all: bool,
    /// Report ignored items as `Ignored` instead of hiding them (`--no-ignore`).
    pub no_ignore: bool,
    /// Patterns of the `global-ignores` setting; the built-in list when `None`.
    pub global_ignores: Option<Vec<String>>,
    /// Repair the stat cache of files proven unchanged. The caller must hold the WC write lock.
    pub update_cache: bool,
    /// Skip comparing file contents (`svn status -q`-style quick mode keeps only recorded state).
    pub ignore_text_mods: bool,
    /// Worker threads; `0` chooses automatically.
    pub threads: usize,
}

impl Default for StatusOptions {
    fn default() -> Self {
        Self {
            get_all: false,
            no_ignore: false,
            global_ignores: None,
            update_cache: false,
            ignore_text_mods: false,
            threads: 0,
        }
    }
}

/// Pristine texts of a working copy, read straight from `.svn/pristine`.
pub struct DbPristineSource<'a>(pub &'a WcDb);

impl PristineSource for DbPristineSource<'_> {
    fn open_text(
        &self,
        sha1: &crate::pristine::Sha1Digest,
    ) -> Result<Box<dyn std::io::Read>, crate::pristine::PristineError> {
        let hex = sha1.to_hex();
        let path = self.0.wcroot().join(ADM_DIR).join("pristine").join(&hex[..2]).join(format!("{hex}.svn-base"));
        std::fs::File::open(&path)
            .map(|f| Box::new(f) as Box<dyn std::io::Read>)
            .map_err(|source| match source.kind() {
                std::io::ErrorKind::NotFound => crate::pristine::PristineError::NotInstalled(*sha1),
                _ => crate::pristine::PristineError::Io { op: "open", path, source },
            })
    }

    fn text_size(&self, sha1: &crate::pristine::Sha1Digest) -> Result<u64, crate::pristine::PristineError> {
        let size: Option<i64> = self
            .0
            .with_read(|c| {
                c.query_row("SELECT size FROM pristine WHERE checksum = ?1", [sha1.to_svn_string()], |r| r.get(0))
                    .map(Some)
                    .or_else(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => Ok(None),
                        other => Err(sql_err(other)),
                    })
            })
            .map_err(|e| crate::pristine::PristineError::index(e.message))?;
        size.and_then(|s| u64::try_from(s).ok()).ok_or(crate::pristine::PristineError::NotInstalled(*sha1))
    }
}

fn basename(relpath: &str) -> &str {
    relpath.rsplit('/').next().unwrap_or(relpath)
}

fn parent_of(relpath: &str) -> &str {
    relpath.rsplit_once('/').map_or("", |(p, _)| p)
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() { name.to_string() } else { format!("{parent}/{name}") }
}

fn rel(path: &str) -> Result<RelPath, SvnError> {
    RelPath::new(path)
}

/// A file whose recorded stat did not match and whose content must be compared.
struct Job {
    slot: usize,
    path: std::path::PathBuf,
    disk: DiskFile,
    recorded: RecordedFile,
}

/// Partial result for a versioned path that still waits for the text check.
struct Pending {
    entry: StatusEntry,
    /// Index into the jobs / verdict table when a text check is needed.
    job: Option<usize>,
    /// The node is the root of its add/delete operation.
    op_root: bool,
}

/// Working copy status of `target` and everything below it, in svn's output order
/// (directories before their children, names compared bytewise per level).
///
/// `pristine` supplies pristine texts for the content comparison (see [`DbPristineSource`]).
/// With `opts.update_cache` the stat cache of proven-unchanged files is written back in one
/// transaction; the caller must hold the working copy write lock.
pub fn status(
    db: &WcDb,
    pristine: &(dyn PristineSource + Sync),
    target: &RelPath,
    opts: &StatusOptions,
    cancel: &CancelToken,
) -> Result<Vec<StatusEntry>, SvnError> {
    let wc_id = db.wc_id();
    let (nodes, actual) = db.with_read(|c| {
        Ok((load::load_nodes(c, wc_id, target)?, load::load_actual(c, wc_id, target)?))
    })?;
    cancel.check()?;

    let target_str = target.as_str();
    if !target.is_root() && !nodes.contains_key(target_str) {
        return Err(SvnError::new(ErrorCode::PathNotFound, format!("'{target_str}' is not a working copy path")));
    }

    // Directories worth entering: every versioned directory that is visible in the working copy.
    let descend: HashSet<String> = nodes
        .iter()
        .filter(|(_, n)| n.kind == NodeKind::Dir && !matches!(n.status, DbStatus::NotPresent | DbStatus::Excluded))
        .map(|(p, _)| p.clone())
        .chain(std::iter::once(target_str.to_string()))
        .collect();
    let fs = scan::scan(db.wcroot(), target, descend, opts.threads)?;
    cancel.check()?;

    let mut ctx = Ctx::new(db, &nodes, &actual, &fs, opts);

    // Every path that may produce an entry, in svn's order (names compared bytewise per level).
    let in_scope = |p: &str| target.is_root() || p == target_str || p.starts_with(&format!("{target_str}/"));
    let mut paths: Vec<&String> = nodes.keys().chain(fs.keys()).chain(actual.keys()).filter(|p| in_scope(p)).collect();
    paths.sort_unstable();
    paths.dedup();
    paths.sort_by(|a, b| a.split('/').cmp(b.split('/')));

    let mut pending: Vec<Pending> = Vec::new();
    let mut jobs: Vec<Job> = Vec::new();
    for path in paths {
        if let Some(p) = ctx.classify(path, target_str, &mut jobs)? {
            pending.push(p);
        }
    }
    cancel.check()?;

    let verdicts = run_jobs(&jobs, pristine, opts.threads, cancel);
    let mut cache_updates: Vec<(String, DiskFile)> = Vec::new();
    let mut out = Vec::with_capacity(pending.len());
    for mut p in pending {
        if let Some(slot) = p.job {
            let verdict = verdicts[slot].clone()?;
            if verdict.modified {
                p.entry.text_status = ItemStatus::Modified;
            }
            if let Some(stat) = verdict.unchanged_stat {
                cache_updates.push((p.entry.path.as_str().to_string(), stat));
            }
        }
        finalize(&mut p.entry);
        if sendable(&p, opts) {
            out.push(p.entry);
        }
    }
    if opts.update_cache && !cache_updates.is_empty() {
        write_cache(db, &cache_updates)?;
    }
    Ok(out)
}

/// Stores `(translated_size, last_mod_time)` for files proven unchanged, in one transaction.
fn write_cache(db: &WcDb, updates: &[(String, DiskFile)]) -> Result<(), SvnError> {
    let wc_id = db.wc_id();
    db.with_write(|tx| {
        let mut stmt = tx
            .prepare(
                "UPDATE nodes SET translated_size = ?3, last_mod_time = ?4 \
                 WHERE wc_id = ?1 AND local_relpath = ?2 \
                 AND op_depth = (SELECT MAX(op_depth) FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2)",
            )
            .map_err(sql_err)?;
        for (path, stat) in updates {
            let size = i64::try_from(stat.size).unwrap_or(i64::MAX);
            stmt.execute(params![wc_id, path, size, stat.mtime_micros]).map_err(sql_err)?;
        }
        Ok(())
    })
}

type Verdicts = Vec<Result<modified::Verdict, SvnError>>;

/// Runs the content comparisons on a small thread pool; `result[slot]` belongs to `jobs[slot]`.
fn run_jobs(jobs: &[Job], pristine: &(dyn PristineSource + Sync), threads: usize, cancel: &CancelToken) -> Verdicts {
    if jobs.is_empty() {
        return Vec::new();
    }
    let workers = if threads == 0 {
        std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
    } else {
        threads
    }
    .min(jobs.len());
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<(usize, Result<modified::Verdict, SvnError>)>();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let next = &next;
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(i) else { break };
                    let result = cancel
                        .check()
                        .and_then(|()| modified::file_modified(&job.path, Some(job.disk), &job.recorded, pristine));
                    if tx.send((job.slot, result)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
    });
    let mut verdicts: Vec<Option<Result<modified::Verdict, SvnError>>> = (0..jobs.len()).map(|_| None).collect();
    for (slot, result) in rx {
        verdicts[slot] = Some(result);
    }
    verdicts
        .into_iter()
        .map(|v| v.unwrap_or_else(|| Err(SvnError::new(ErrorCode::Internal, "status worker did not finish"))))
        .collect()
}

/// Shared read-only state of a status run plus the per-directory ignore cache.
struct Ctx<'a> {
    db: &'a WcDb,
    nodes: &'a HashMap<String, NodeRec>,
    actual: &'a HashMap<String, ActualRec>,
    fs: &'a HashMap<String, FsEntry>,
    opts: &'a StatusOptions,
    defaults: Vec<String>,
    /// svn:ignore + svn:global-ignores per directory (own, not inherited).
    own_ignores: HashMap<String, (Vec<String>, Vec<String>)>,
    patterns: HashMap<String, Vec<String>>,
    repos_relpaths: HashMap<String, Option<String>>,
    next_slot: usize,
}

impl<'a> Ctx<'a> {
    fn new(
        db: &'a WcDb,
        nodes: &'a HashMap<String, NodeRec>,
        actual: &'a HashMap<String, ActualRec>,
        fs: &'a HashMap<String, FsEntry>,
        opts: &'a StatusOptions,
    ) -> Self {
        let defaults = opts.global_ignores.clone().unwrap_or_else(|| {
            ignores::DEFAULT_GLOBAL_IGNORES.iter().map(|s| (*s).to_string()).collect()
        });
        Self {
            db,
            nodes,
            actual,
            fs,
            opts,
            defaults,
            own_ignores: HashMap::new(),
            patterns: HashMap::new(),
            repos_relpaths: HashMap::new(),
            next_slot: 0,
        }
    }

    /// `svn:ignore` and `svn:global-ignores` of one directory.
    fn own_ignores(&mut self, dir: &str) -> Result<&(Vec<String>, Vec<String>), SvnError> {
        if !self.own_ignores.contains_key(dir) {
            let mut local = Vec::new();
            let mut inheritable = Vec::new();
            if self.nodes.get(dir).is_some_and(|n| n.status != DbStatus::NotPresent) || dir.is_empty() {
                let props = self.db.actual_props(&rel(dir)?)?;
                for (name, value) in props {
                    match name.as_slice() {
                        b"svn:ignore" => local.extend(ignores::split_patterns(&value)),
                        b"svn:global-ignores" => inheritable.extend(ignores::split_patterns(&value)),
                        _ => {}
                    }
                }
            }
            self.own_ignores.insert(dir.to_string(), (local, inheritable));
        }
        Ok(&self.own_ignores[dir])
    }

    /// Patterns that apply to the unversioned children of `dir` (`collect_ignore_patterns`):
    /// the defaults, the `svn:global-ignores` of `dir` and all its ancestors, and `dir`'s own
    /// `svn:ignore` (which does not inherit).
    fn patterns_for(&mut self, dir: &str) -> Result<Vec<String>, SvnError> {
        if let Some(p) = self.patterns.get(dir) {
            return Ok(p.clone());
        }
        let mut chain = vec![dir.to_string()];
        let mut cur = dir.to_string();
        while !cur.is_empty() {
            cur = parent_of(&cur).to_string();
            chain.push(cur.clone());
        }
        let mut patterns = self.defaults.clone();
        for ancestor in chain.iter().rev() {
            let (_, inheritable) = self.own_ignores(ancestor)?.clone();
            patterns.extend(inheritable);
        }
        let (local, _) = self.own_ignores(dir)?.clone();
        patterns.extend(local);
        self.patterns.insert(dir.to_string(), patterns.clone());
        Ok(patterns)
    }

    /// Repository path of a node: its own, or the parent's plus its name (plain additions).
    fn repos_relpath(&mut self, path: &str) -> Option<String> {
        if let Some(r) = self.repos_relpaths.get(path) {
            return r.clone();
        }
        let own = self.nodes.get(path).and_then(|n| n.repos_relpath.clone());
        let value = match own {
            Some(r) => Some(r),
            None if path.is_empty() => None,
            None => self.repos_relpath(parent_of(path)).map(|p| join(&p, basename(path))),
        };
        self.repos_relpaths.insert(path.to_string(), value.clone());
        value
    }

    fn switched(&mut self, path: &str, info: &NodeRec) -> bool {
        let (Some(own), true) = (&info.repos_relpath, !path.is_empty()) else { return false };
        let Some(parent) = self.repos_relpath(parent_of(path)) else { return false };
        let name = own.strip_prefix(&parent).and_then(|r| {
            if parent.is_empty() { Some(r) } else { r.strip_prefix('/') }
        });
        name.is_none_or(|n| n != basename(path))
    }

    /// Conflict state with marker existence checks (`svn_wc__internal_conflicted_p`).
    fn conflicts(&self, path: &str) -> (bool, bool, bool) {
        let Some(skel) = self.actual.get(path).and_then(|a| a.conflict.as_ref()) else { return (false, false, false) };
        let root = self.db.wcroot();
        let exists = |marker: &Option<String>| marker.as_ref().is_some_and(|m| root.join(m).is_file());
        let text = skel.text().is_some_and(|t| exists(&t.mine) || exists(&t.their) || exists(&t.original));
        let prop = skel.prop().is_some_and(|p| p.markers.iter().any(|m| root.join(m).is_file()));
        let tree = skel.tree().is_some_and(|t| !(t.reason == TreeReason::MovedAway && t.action == TreeAction::Edit));
        (text, prop, tree)
    }

    /// Decides what to report for `path` (`one_child_status`); versioned files that need a
    /// content check are queued in `jobs`.
    fn classify(&mut self, path: &str, target: &str, jobs: &mut Vec<Job>) -> Result<Option<Pending>, SvnError> {
        let info = self.nodes.get(path);
        let dirent = self.fs.get(path);
        let (text_c, prop_c, tree_c) = self.conflicts(path);
        let conflicted = text_c || prop_c || tree_c;

        if let Some(info) = info
            && !matches!(info.status, DbStatus::NotPresent | DbStatus::Excluded)
            && !(info.kind == NodeKind::None && info.status == DbStatus::Normal)
        {
            return self.versioned(path, info, dirent, (text_c, prop_c, tree_c), jobs).map(Some);
        }
        if !conflicted {
            if dirent.is_none() && path != target {
                return Ok(None); // marker node
            }
            if basename(path) == ADM_DIR {
                return Ok(None);
            }
        }
        if path.is_empty() {
            return Ok(None);
        }
        // Unversioned (or a tree-conflict victim without a node).
        let ignored = if dirent.is_some() {
            let patterns = self.patterns_for(parent_of(path))?;
            ignores::is_ignored(basename(path), &patterns)
        } else {
            false
        };
        let node_status = match dirent {
            Some(_) if ignored => ItemStatus::Ignored,
            Some(_) => ItemStatus::Unversioned,
            None if tree_c => ItemStatus::Conflicted,
            None => ItemStatus::None,
        };
        let entry = StatusEntry {
            path: rel(path)?,
            versioned: false,
            kind: NodeKind::None,
            node_status,
            text_status: ItemStatus::None,
            prop_status: ItemStatus::None,
            copied: false,
            switched: false,
            file_external: false,
            revision: None,
            changed_rev: None,
            changed_author: None,
            changed_date: None,
            changelist: None,
            text_conflicted: false,
            prop_conflicted: false,
            tree_conflicted: tree_c,
            moved_to: None,
        };
        Ok(Some(Pending { entry, job: None, op_root: false }))
    }

    /// `assemble_status` for a versioned path.
    fn versioned(
        &mut self,
        path: &str,
        info: &NodeRec,
        dirent: Option<&FsEntry>,
        conflicts: (bool, bool, bool),
        jobs: &mut Vec<Job>,
    ) -> Result<Pending, SvnError> {
        let actual = self.actual.get(path);
        let props_mod = actual.is_some_and(|a| a.props_mod);
        let switched = self.switched(path, info);
        let mut copied = false;
        let mut node_status = ItemStatus::Normal;
        let mut text_status = ItemStatus::Normal;
        let mut prop_status = ItemStatus::None;
        let mut job = None;

        if info.status == DbStatus::Incomplete || info.incomplete {
            node_status = ItemStatus::Incomplete;
        } else if info.status == DbStatus::Deleted {
            node_status = ItemStatus::Deleted;
            copied = !info.have_base || info.have_more_work || info.copied;
        } else {
            let expected = if info.kind == NodeKind::Dir { FsKind::Dir } else { FsKind::File };
            let on_disk_kind = dirent.map(|d| if d.kind == FsKind::Symlink { FsKind::File } else { d.kind });
            if on_disk_kind != Some(expected) {
                node_status = if dirent.is_none() { ItemStatus::Missing } else { ItemStatus::Obstructed };
            }
        }

        if info.status != DbStatus::Deleted {
            if props_mod {
                prop_status = ItemStatus::Modified;
            } else if info.had_props {
                prop_status = ItemStatus::Normal;
            }
        }

        let is_file = matches!(info.kind, NodeKind::File | NodeKind::Symlink);
        if info.kind != NodeKind::Dir && node_status == ItemStatus::Normal {
            let disk_special = dirent.is_some_and(|d| d.kind == FsKind::Symlink);
            let special_matches = !cfg!(unix) || info.special == disk_special;
            if is_file && special_matches {
                if info.checksum.is_none() {
                    text_status = ItemStatus::Modified; // local addition without pristine
                } else if !self.opts.ignore_text_mods
                    && let Some(d) = dirent
                {
                    let disk = DiskFile { size: d.size, mtime_micros: d.mtime_micros };
                    let cached = info.translated_size.is_some_and(|s| u64::try_from(s).is_ok_and(|s| s == d.size))
                        && info.last_mod_time.is_some_and(|m| m != 0 && m == d.mtime_micros);
                    if !cached {
                        let translate = if info.had_props || props_mod {
                            let props = self.db.actual_props(&rel(path)?)?;
                            Some(TranslateInfo::from_props(|name| {
                                props.iter().find(|(n, _)| n.as_slice() == name.as_bytes()).map(|(_, v)| v.as_slice())
                            }))
                        } else {
                            None
                        };
                        let slot = self.next_slot;
                        self.next_slot += 1;
                        jobs.push(Job {
                            slot,
                            path: self.db.wcroot().join(path),
                            disk,
                            recorded: RecordedFile {
                                checksum: info.checksum,
                                size: info.translated_size,
                                mtime_micros: info.last_mod_time,
                                translate,
                            },
                        });
                        job = Some(slot);
                    }
                }
            } else if !special_matches {
                node_status = ItemStatus::Obstructed;
            }
        }

        let (text_c, prop_c, tree_c) = conflicts;
        let mut moved_to_path = None;
        if node_status == ItemStatus::Normal && info.status == DbStatus::Added {
            copied = info.copied;
            if !info.op_root {
                // Keep status normal.
            } else if !info.have_base && !info.have_more_work {
                node_status = ItemStatus::Added;
            } else {
                node_status = match info.below_working {
                    Some(Presence::NotPresent | Presence::BaseDeleted) => ItemStatus::Added,
                    _ => ItemStatus::Replaced,
                };
            }
        }
        if let Some(m) = &info.moved_to {
            moved_to_path = Some(rel(m)?);
        }

        let entry = StatusEntry {
            path: rel(path)?,
            versioned: true,
            kind: if info.kind == NodeKind::Symlink { NodeKind::File } else { info.kind },
            node_status,
            text_status,
            prop_status,
            copied,
            switched,
            file_external: info.file_external,
            revision: if info.status == DbStatus::Added { None } else { info.base_revision },
            changed_rev: info.changed_rev,
            changed_author: info.changed_author.clone(),
            changed_date: info.changed_date,
            changelist: actual.and_then(|a| a.changelist.clone()),
            text_conflicted: text_c,
            prop_conflicted: prop_c,
            tree_conflicted: tree_c,
            moved_to: moved_to_path,
        };
        Ok(Pending { entry, job, op_root: info.op_root })
    }

}

/// Final precedence rules of `assemble_status` (text, then props, replace a normal node status)
/// and the promotion done by the svn client library: a conflict whose marker file still exists
/// turns the corresponding status into `Conflicted`.
fn finalize(entry: &mut StatusEntry) {
    if !entry.versioned {
        return;
    }
    if entry.node_status == ItemStatus::Normal {
        entry.node_status = entry.text_status;
    }
    if entry.node_status == ItemStatus::Normal && entry.prop_status != ItemStatus::None {
        entry.node_status = entry.prop_status;
    }
    if entry.text_conflicted {
        entry.text_status = ItemStatus::Conflicted;
    }
    if entry.prop_conflicted {
        entry.prop_status = ItemStatus::Conflicted;
    }
    if entry.text_conflicted || entry.prop_conflicted {
        entry.node_status = ItemStatus::Conflicted;
    }
}

/// Which entries a default `svn status` reports: the filter at the end of `assemble_status`
/// (`get_all = false`) and `send_unversioned_item`.
fn sendable(p: &Pending, opts: &StatusOptions) -> bool {
    let e = &p.entry;
    if !e.versioned {
        return match e.node_status {
            ItemStatus::None => false,
            ItemStatus::Ignored => opts.no_ignore,
            _ => true,
        };
    }
    if opts.get_all {
        return true;
    }
    let boring = matches!(e.node_status, ItemStatus::None | ItemStatus::Normal)
        || (e.node_status == ItemStatus::Deleted && !p.op_root);
    !(boring
        && !e.switched
        && e.changelist.is_none()
        && !(e.text_conflicted || e.prop_conflicted || e.tree_conflicted)
        && e.moved_to.is_none())
}

#[cfg(test)]
mod tests;

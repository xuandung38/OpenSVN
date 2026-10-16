//! `svn diff --summarize`: which nodes differ between two repository locations, or between a
//! working copy (with its local changes) and a repository location, without file contents.
//!
//! The repository side is one diff drive ([`UpdateMode::Diff`]): the server compares the
//! reported tree with the other URL and the editor only notes what it is told. Like svn 1.15:
//!
//! - ancestry is ignored unless asked for (a replaced file that stays a file is "modified");
//! - a deleted directory is listed together with everything it held;
//! - an added directory comes with everything it holds (the server adds each node);
//! - only regular properties count (`svn:entry:*`, `svn:wc:*` and DAV properties do not).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use svn_ra_api::{RaSession, UpdateMode, UpdateRequest};
use svn_types::{
    CancelToken, Checksum, DeltaEditor, Depth, DirId, ErrorCode, FileId, NodeKind, NodeStatus,
    PropChange, PropStatus, RelPath, RepoUrl, Reporter, Result, Revnum, SvnError, WcStatus,
    WindowSink,
};
use svn_wc::db::WcDb;

use crate::ctx::ClientCtx;
use crate::status::{StatusOptions, local_status};
use crate::update::{anchor_of, build_report, root_string};
use crate::wc::{self, join_rel};

/// How a node differs, read from the left side to the right side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SummaryKind {
    /// Same node on both sides; only its properties differ (see [`SummaryEntry::props_changed`]).
    Normal,
    /// Only on the right side.
    Added,
    /// Only on the left side.
    Deleted,
    /// On both sides with different text (files).
    Modified,
}

/// One node that differs (`svn_client_diff_summarize_t`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryEntry {
    /// Path below the compared locations; the root is the empty path.
    pub path: RelPath,
    /// Node kind (of the side that has the node; the right one when both have it).
    pub node_kind: NodeKind,
    /// What differs.
    pub kind: SummaryKind,
    /// Regular properties differ.
    pub props_changed: bool,
}

/// Options of a summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryOptions {
    /// How deep to compare (`Unknown` means infinity).
    pub depth: Depth,
    /// `false` treats a replaced node as deleted and added (`svn diff --notice-ancestry`).
    pub ignore_ancestry: bool,
}

impl Default for SummaryOptions {
    fn default() -> Self {
        Self {
            depth: Depth::Infinity,
            ignore_ancestry: true,
        }
    }
}

/// Result of a summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Revision the right side was read at.
    pub right_rev: Revnum,
    /// Differing nodes, sorted by path (a node both deleted and added keeps both entries).
    pub entries: Vec<SummaryEntry>,
}

fn is_regular_prop(name: &str) -> bool {
    !name.starts_with("svn:entry:") && !name.starts_with("svn:wc:") && !name.starts_with("svn:dav:")
}

fn depth_of(d: Depth) -> Depth {
    match d {
        Depth::Unknown | Depth::Exclude => Depth::Infinity,
        d => d,
    }
}

/// The depth a working-copy comparison asks the server for. Like svn (`libsvn_client/diff.c`), a
/// full depth is sent as `unknown`, so the report's recorded depths hold: `infinity` would make
/// the server treat every sparse folder as a depth upgrade and send what it leaves out as added.
fn wc_request_depth(d: Depth) -> Depth {
    match depth_of(d) {
        Depth::Infinity => Depth::Unknown,
        d => d,
    }
}

#[derive(Default)]
struct Collected {
    revision: Option<Revnum>,
    entries: Vec<SummaryEntry>,
    /// Deleted paths; their kinds (and what a deleted directory held) are read afterwards.
    deleted: Vec<RelPath>,
}

struct OpenNode {
    path: RelPath,
    kind: NodeKind,
    added: bool,
    text: bool,
    props: bool,
}

/// Editor of a diff drive: notes every node the server reports as different.
struct SummaryEditor {
    out: Arc<Mutex<Collected>>,
    open: HashMap<u32, OpenNode>,
    next: u32,
}

struct NoWindows;
impl WindowSink for NoWindows {
    fn window(&mut self, _: Option<&svn_types::TxDeltaWindow>) -> Result<()> {
        Ok(())
    }
}

impl SummaryEditor {
    fn push(&mut self, path: &RelPath, kind: NodeKind, added: bool) -> u32 {
        self.next += 1;
        self.open.insert(
            self.next,
            OpenNode {
                path: path.clone(),
                kind,
                added,
                text: false,
                props: false,
            },
        );
        self.next
    }

    fn node(&mut self, id: u32) -> Result<&mut OpenNode> {
        self.open
            .get_mut(&id)
            .ok_or_else(|| SvnError::new(ErrorCode::Internal, "the drive used an unknown node"))
    }

    fn close(&mut self, id: u32) -> Result<()> {
        let n = self.open.remove(&id).ok_or_else(|| {
            SvnError::new(ErrorCode::Internal, "the drive closed an unknown node")
        })?;
        let kind = if n.added {
            Some(SummaryKind::Added)
        } else if n.text {
            Some(SummaryKind::Modified)
        } else if n.props {
            Some(SummaryKind::Normal)
        } else {
            None
        };
        if let Some(kind) = kind {
            self.out
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entries
                .push(SummaryEntry {
                    path: n.path,
                    node_kind: n.kind,
                    kind,
                    // Like svn: an added node is "added", whatever properties it has.
                    props_changed: n.props && !n.added,
                });
        }
        Ok(())
    }
}

impl DeltaEditor for SummaryEditor {
    fn set_target_revision(&mut self, rev: Revnum) -> Result<()> {
        self.out
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .revision = Some(rev);
        Ok(())
    }
    fn open_root(&mut self, _: Option<Revnum>) -> Result<DirId> {
        Ok(DirId(self.push(&RelPath::root(), NodeKind::Dir, false)))
    }
    fn delete_entry(&mut self, path: &RelPath, _: Option<Revnum>, _: DirId) -> Result<()> {
        self.out
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .deleted
            .push(path.clone());
        Ok(())
    }
    fn add_directory(
        &mut self,
        path: &RelPath,
        _: DirId,
        _: Option<(&str, Revnum)>,
    ) -> Result<DirId> {
        Ok(DirId(self.push(path, NodeKind::Dir, true)))
    }
    fn open_directory(&mut self, path: &RelPath, _: DirId, _: Option<Revnum>) -> Result<DirId> {
        Ok(DirId(self.push(path, NodeKind::Dir, false)))
    }
    fn change_dir_prop(&mut self, dir: DirId, change: PropChange) -> Result<()> {
        if is_regular_prop(&change.name) {
            self.node(dir.0)?.props = true;
        }
        Ok(())
    }
    fn close_directory(&mut self, dir: DirId) -> Result<()> {
        self.close(dir.0)
    }
    fn absent_directory(&mut self, _: &RelPath, _: DirId) -> Result<()> {
        Ok(())
    }
    fn add_file(&mut self, path: &RelPath, _: DirId, _: Option<(&str, Revnum)>) -> Result<FileId> {
        Ok(FileId(self.push(path, NodeKind::File, true)))
    }
    fn open_file(&mut self, path: &RelPath, _: DirId, _: Option<Revnum>) -> Result<FileId> {
        Ok(FileId(self.push(path, NodeKind::File, false)))
    }
    fn apply_textdelta(
        &mut self,
        file: FileId,
        _: Option<Checksum>,
    ) -> Result<Box<dyn WindowSink>> {
        self.node(file.0)?.text = true;
        Ok(Box::new(NoWindows))
    }
    fn change_file_prop(&mut self, file: FileId, change: PropChange) -> Result<()> {
        if is_regular_prop(&change.name) {
            self.node(file.0)?.props = true;
        }
        Ok(())
    }
    fn close_file(&mut self, file: FileId, _: Option<Checksum>) -> Result<()> {
        self.close(file.0)
    }
    fn absent_file(&mut self, _: &RelPath, _: DirId) -> Result<()> {
        Ok(())
    }
    fn close_edit(&mut self) -> Result<()> {
        Ok(())
    }
    fn abort_edit(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Runs a diff drive to `right` and returns what the editor noted, in drive order.
///
/// Nothing is written, so a drive that broke off because the connection failed (see
/// [`svn_types::ConnectionFailure`]) is run again from the start, with a fresh report from
/// `report`, after the delays of [`crate::update::RECONNECT_DELAYS`] (like `status -u`).
async fn drive(
    session: &dyn RaSession,
    target: RelPath,
    report: &(dyn Fn() -> svn_ra_api::ReportFn + Send + Sync),
    right: &RepoUrl,
    right_rev: Option<Revnum>,
    opts: SummaryOptions,
    cancel: &CancelToken,
) -> Result<Collected> {
    let mut attempt = 0usize;
    loop {
        let err = match drive_once(
            session,
            target.clone(),
            report(),
            right,
            right_rev,
            opts,
            cancel,
        )
        .await
        {
            Ok(c) => return Ok(c),
            Err(e) => e,
        };
        let delay = crate::update::RECONNECT_DELAYS.get(attempt).copied();
        let (Some(delay), Some(failure)) = (delay, err.connection_failure()) else {
            return Err(err);
        };
        if err.code == ErrorCode::Cancelled {
            return Err(err);
        }
        attempt += 1;
        tracing::info!(?failure, attempt, "comparison broke off; asking again");
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                cancel.check()?;
            }
            () = tokio::time::sleep(delay) => {}
        }
    }
}

async fn drive_once(
    session: &dyn RaSession,
    target: RelPath,
    report: svn_ra_api::ReportFn,
    right: &RepoUrl,
    right_rev: Option<Revnum>,
    opts: SummaryOptions,
    cancel: &CancelToken,
) -> Result<Collected> {
    let out = Arc::new(Mutex::new(Collected::default()));
    let editor = SummaryEditor {
        out: Arc::clone(&out),
        open: HashMap::new(),
        next: 0,
    };
    session
        .do_update(
            UpdateRequest {
                revision: right_rev,
                target,
                // As the caller wants it on the wire (`wc_request_depth`).
                depth: opts.depth,
                mode: UpdateMode::Diff(right.clone()),
                send_copyfrom: false,
                ignore_ancestry: opts.ignore_ancestry,
            },
            report,
            Box::new(editor),
            cancel,
        )
        .await?;
    let mut c = out.lock().unwrap_or_else(PoisonError::into_inner);
    Ok(std::mem::take(&mut *c))
}

/// Everything below directory `path` of `session` at `rev` (not `path` itself), depth first.
async fn list_tree(
    session: &dyn RaSession,
    path: &RelPath,
    rev: Revnum,
    cancel: &CancelToken,
) -> Result<Vec<(RelPath, NodeKind)>> {
    let mut out = Vec::new();
    let mut todo = vec![path.clone()];
    while let Some(dir) = todo.pop() {
        cancel.check()?;
        for e in session
            .list(&dir, Some(rev), Depth::Immediates, cancel)
            .await?
        {
            let child = join_rel(&dir, &RelPath::from_server(&e.name)?)?;
            if e.kind == NodeKind::Dir {
                todo.push(child.clone());
            }
            out.push((child, e.kind));
        }
    }
    Ok(out)
}

/// By path; a replaced node lists its deletion before its addition.
fn sorted(mut entries: Vec<SummaryEntry>) -> Vec<SummaryEntry> {
    entries.sort_by(|a, b| {
        (a.path.as_str(), a.kind == SummaryKind::Added)
            .cmp(&(b.path.as_str(), b.kind == SummaryKind::Added))
    });
    entries.dedup();
    entries
}

/// What differs between the session's URL at `left_rev` and `right` at `right_rev` (`None`:
/// HEAD), like `svn diff --summarize LEFT@left_rev RIGHT@right_rev`. Paths are relative to the
/// two locations. Both must be in the session's repository.
pub async fn summarize_repos(
    session: &dyn RaSession,
    left_rev: Revnum,
    right: &RepoUrl,
    right_rev: Option<Revnum>,
    opts: SummaryOptions,
    cancel: &CancelToken,
) -> Result<Summary> {
    session.ensure_authenticated(cancel).await?;
    let left_kind = session
        .check_path(&RelPath::root(), Some(left_rev), cancel)
        .await?;
    if left_kind != NodeKind::Dir {
        return Err(SvnError::new(
            ErrorCode::PathNotFound,
            format!(
                "{} is not a folder in r{}",
                session.session_url(),
                left_rev.0
            ),
        ));
    }
    let depth = depth_of(opts.depth);
    let report = move || -> svn_ra_api::ReportFn {
        Box::new(move |r: &mut dyn Reporter| {
            r.set_path(&RelPath::root(), left_rev, depth, false, None)?;
            r.finish()
        })
    };
    let c = drive(
        session,
        RelPath::root(),
        &report,
        right,
        right_rev,
        SummaryOptions { depth, ..opts },
        cancel,
    )
    .await?;
    let mut entries = c.entries;
    for path in c.deleted {
        let kind = session.check_path(&path, Some(left_rev), cancel).await?;
        if kind == NodeKind::Dir && depth == Depth::Infinity {
            for (p, k) in list_tree(session, &path, left_rev, cancel).await? {
                entries.push(SummaryEntry {
                    path: p,
                    node_kind: k,
                    kind: SummaryKind::Deleted,
                    props_changed: false,
                });
            }
        }
        entries.push(SummaryEntry {
            path,
            node_kind: kind,
            kind: SummaryKind::Deleted,
            props_changed: false,
        });
    }
    let right_rev = match (c.revision, right_rev) {
        (Some(r), _) | (None, Some(r)) => r,
        (None, None) => session.youngest_rev(cancel).await?,
    };
    Ok(Summary {
        right_rev,
        entries: sorted(entries),
    })
}

/// The local side of one path: what the working copy has, compared with its BASE.
#[derive(Debug, Clone, Copy)]
struct Local {
    kind: NodeKind,
    /// Kind in BASE when it can differ from `kind` (a replaced node); `kind` otherwise.
    base_kind: NodeKind,
    status: NodeStatus,
    props: bool,
}

fn local_of(db: &WcDb, s: &WcStatus) -> Result<Local> {
    let base_kind = if s.text == NodeStatus::Replaced {
        db.read_base(&s.path)?.map_or(s.kind, |n| n.kind)
    } else {
        s.kind
    };
    Ok(Local {
        kind: s.kind,
        base_kind,
        status: s.text,
        props: matches!(s.props, PropStatus::Modified | PropStatus::Conflicted),
    })
}

/// The node exists in the working tree (scheduled additions included).
fn in_working(l: Option<&Local>) -> bool {
    l.is_some_and(|l| {
        !matches!(
            l.status,
            NodeStatus::Deleted | NodeStatus::None | NodeStatus::Unversioned | NodeStatus::Ignored
        )
    })
}

/// What differs between the working copy at `wc_path` (local changes included) and `right` at
/// `right_rev` (`None`: HEAD), like `svn diff --summarize --old=WC --new=RIGHT@right_rev`:
/// "added" is only in `right`, "deleted" only in the working copy. Paths are relative to
/// `wc_path`. The session must be at the URL of the working copy root or above it.
///
/// The repository side compares the working copy's BASE (mixed revisions and sparse depths as
/// recorded) with `right`; each node is then combined with its local change. A node changed on
/// both sides counts as modified even when both changes happen to be the same.
pub async fn summarize_wc(
    ctx: &ClientCtx,
    session: &dyn RaSession,
    wc_path: &Path,
    right: &RepoUrl,
    right_rev: Option<Revnum>,
    opts: SummaryOptions,
    cancel: &CancelToken,
) -> Result<Summary> {
    session.ensure_authenticated(cancel).await?;
    let (owner, p) = (ctx.lock_owner.clone(), wc_path.to_path_buf());
    let (db, rel) = tokio::task::spawn_blocking(move || wc::open_containing(&owner, &p))
        .await
        .map_err(|e| SvnError::new(ErrorCode::Internal, format!("worker failed: {e}")))??;
    let db = Arc::new(db);
    let anchor = anchor_of(&db, session)?;
    let target = wc::strip_rel(&rel, &anchor).ok_or_else(|| {
        SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!("'{rel}' is not below the session URL"),
        )
    })?;
    let depth = depth_of(opts.depth);
    // Unmodified nodes too: they say what a directory deleted on the right held.
    let locals: BTreeMap<RelPath, Local> = {
        let status_opts = StatusOptions {
            depth,
            show_unmodified: true,
            include_unversioned: false,
            include_ignored: false,
            include_needs_lock: false,
        };
        let (db, rel, cancel, globals) = (
            Arc::clone(&db),
            rel.clone(),
            cancel.clone(),
            ctx.config.global_ignores.clone(),
        );
        tokio::task::spawn_blocking(move || {
            local_status(&db, &rel, &status_opts, globals.as_deref(), &cancel)?
                .iter()
                .map(|s| Ok((s.path.clone(), local_of(&db, s)?)))
                .collect::<Result<BTreeMap<_, _>>>()
        })
        .await
        .map_err(|e| SvnError::new(ErrorCode::Internal, format!("worker failed: {e}")))??
    };
    let root = root_string(session);
    let report = || {
        build_report(
            Arc::clone(&db),
            anchor.clone(),
            target.clone(),
            depth,
            false,
            root.clone(),
        )
    };
    let c = drive(
        session,
        target.clone(),
        &report,
        right,
        right_rev,
        SummaryOptions {
            depth: wc_request_depth(opts.depth),
            ..opts
        },
        cancel,
    )
    .await?;

    // The repository side by working copy path (BASE -> right); a path deleted and added
    // (replaced on the right with ancestry noticed) keeps both.
    let mut remote: BTreeMap<RelPath, Vec<SummaryEntry>> = BTreeMap::new();
    for mut e in c.entries {
        e.path = join_rel(&anchor, &e.path)?;
        remote.entry(e.path.clone()).or_default().push(e);
    }
    for path in c.deleted {
        let path = join_rel(&anchor, &path)?;
        let kind = locals.get(&path).map_or(NodeKind::Unknown, |l| l.kind);
        let below: Vec<(RelPath, NodeKind)> = locals
            .iter()
            .filter(|(p, _)| wc::strip_rel(p, &path).is_some_and(|r| !r.is_root()))
            .map(|(p, l)| (p.clone(), l.kind))
            .collect();
        for (p, k) in below.into_iter().chain([(path, kind)]) {
            remote.entry(p.clone()).or_default().insert(
                0,
                SummaryEntry {
                    path: p,
                    node_kind: k,
                    kind: SummaryKind::Deleted,
                    props_changed: false,
                },
            );
        }
    }

    let mut entries = Vec::new();
    let paths: std::collections::BTreeSet<&RelPath> = remote
        .keys()
        .chain(
            locals
                .iter()
                .filter(|(_, l)| is_local_change(l))
                .map(|(p, _)| p),
        )
        .collect();
    for path in paths {
        let local = locals.get(path);
        let rem = remote.get(path).map(Vec::as_slice).unwrap_or(&[]);
        combine(path, local, rem, &mut entries);
    }
    // Relative to the requested path.
    let mut out = Vec::with_capacity(entries.len());
    for mut e in entries {
        if let Some(r) = wc::strip_rel(&e.path, &rel) {
            e.path = r;
            out.push(e);
        }
    }
    let right_rev = match (c.revision, right_rev) {
        (Some(r), _) | (None, Some(r)) => r,
        (None, None) => session.youngest_rev(cancel).await?,
    };
    Ok(Summary {
        right_rev,
        entries: sorted(out),
    })
}

fn is_local_change(l: &Local) -> bool {
    l.props
        || matches!(
            l.status,
            NodeStatus::Added
                | NodeStatus::Deleted
                | NodeStatus::Replaced
                | NodeStatus::Modified
                | NodeStatus::Conflicted
        )
}

/// Left = working copy, right = repository: one path's entries from its local change (BASE ->
/// working) and its repository side (BASE -> right).
fn combine(
    path: &RelPath,
    local: Option<&Local>,
    remote: &[SummaryEntry],
    out: &mut Vec<SummaryEntry>,
) {
    let entry = |node_kind, kind, props_changed| SummaryEntry {
        path: path.clone(),
        node_kind,
        kind,
        props_changed,
    };
    let on_right = remote.last();
    let Some(l) = local.filter(|l| is_local_change(l)) else {
        // No local change: the repository side as it is.
        out.extend(remote.iter().cloned());
        return;
    };
    // BASE has every node of the working copy but a plain addition; with no repository-side
    // change, the right side has exactly what BASE has (its kind too: a local replacement may
    // have changed it).
    let base_has = l.status != NodeStatus::Added;
    let right_has = match on_right.map(|e| e.kind) {
        Some(SummaryKind::Deleted) => false,
        Some(_) => true,
        None => base_has,
    };
    let left_has = in_working(Some(l));
    let kind_right = on_right.map_or(l.base_kind, |e| e.node_kind);
    match (left_has, right_has) {
        (false, false) => {}
        (true, false) => out.push(entry(l.kind, SummaryKind::Deleted, false)),
        (false, true) => out.push(entry(kind_right, SummaryKind::Added, false)),
        (true, true) => {
            if l.kind != kind_right && kind_right != NodeKind::Unknown {
                out.push(entry(l.kind, SummaryKind::Deleted, false));
                out.push(entry(kind_right, SummaryKind::Added, false));
                return;
            }
            let remote_text = on_right
                .is_some_and(|e| matches!(e.kind, SummaryKind::Modified | SummaryKind::Added));
            let remote_props = on_right.is_some_and(|e| e.props_changed);
            let local_text = matches!(
                l.status,
                NodeStatus::Added
                    | NodeStatus::Replaced
                    | NodeStatus::Modified
                    | NodeStatus::Conflicted
            );
            let text = l.kind != NodeKind::Dir && (local_text || remote_text);
            let props = l.props || remote_props;
            if text {
                out.push(entry(l.kind, SummaryKind::Modified, props));
            } else if props {
                out.push(entry(l.kind, SummaryKind::Normal, true));
            }
        }
    }
}

//! `svn:externals`: fetching, updating and removing the externals a working copy defines, the
//! way svn 1.15 does after a checkout, update or switch (`svn_client__handle_externals`).
//!
//! After the main drive, the definitions of every versioned directory at or below the target
//! are read (the local value wins over the committed one, as in svn). Each definition is
//! fetched: a directory external becomes a separate working copy registered in the
//! `EXTERNALS` table, a file external a BASE file marked `file_external` in the defining working
//! copy. Externals recorded in the `EXTERNALS` table whose definition is gone are removed
//! afterwards.
//!
//! # Safety
//! - Targets are validated as untrusted repository text (no `..`, no absolute paths, no
//!   administrative directory names) and every existing directory on the way to a target must
//!   be a real directory, never a symbolic link, so an external cannot write outside the working
//!   copy.
//! - Every session is opened through [`SessionOpener`], so an external on another host goes
//!   through the same TLS and authentication policy as the main session.
//! - Removal never deletes local changes. A directory external with local modifications (or
//!   whose nested externals have them) is left exactly as it is, still a working copy; its
//!   unversioned items always survive. A file external with local changes stays on disk as an
//!   unversioned file. svn 1.15 deletes such a modified file external; this client does not.
//! - Externals are bounded. An external whose URL is (or contains) its defining directory or a
//!   directory whose definition led to it is refused as a cycle. One operation processes at
//!   most [`MAX_EXTERNALS`] externals across every nesting level, at most [`MAX_NESTING`] deep.
//!   svn has neither bound, so a definition that includes itself makes it recurse; the cycle
//!   check is conservative and also refuses a pinned revision of such a URL.
//! - A failure of one external is reported in [`ExternalReport`] and the others are still
//!   processed, like svn's `W205011` warnings. Cancellation stops everything.
//!
//! # Known differences from svn 1.15
//! - Revision dates (`-r {2020-01-01}`) are refused (svn resolves them).
//! - With different peg and operative revisions (`-r 5 URL@10`) the URL is used at the operative
//!   revision as written; svn first traces the node's history from the peg revision.
//! - A local change in a file external is merged against the file's old text, like any other
//!   file. svn 1.15 merges file externals against an empty base, so every local change
//!   conflicts there.
//! - The inherited-properties cache of a file external is recorded empty; svn fills it with the
//!   properties of the file's repository ancestors.
//! - Updating a file external on its own is refused; update the directory that defines it.

mod dir;
mod file;
mod parse;
mod remove;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use svn_ra_api::RaSession;
use svn_types::{CancelToken, Depth, ErrorCode, NodeKind, RelPath, RepoUrl, Result, SvnError};
use svn_wc::db::{External, ObtainMode, WcDb};

pub use parse::{ExtRev, ExternalItem, parse_externals, resolve_url};

use crate::ctx::{ClientCtx, ExternalAction, ExternalReport, UpdateEvent, UpdateSummary};
use crate::wc;

/// Opens repository sessions for externals.
///
/// An external may live in another repository, on another host. Implementations must apply the
/// same policy as for the main session (TLS default-deny, saved credentials, prompts) and return
/// a session anchored exactly at `url` (not at the repository root).
#[async_trait]
pub trait SessionOpener: Send + Sync {
    /// An authenticated session anchored at `url`.
    async fn open(&self, url: &RepoUrl, cancel: &CancelToken) -> Result<Arc<dyn RaSession>>;
}

/// Externals inside externals are followed this many levels deep at most, so a definition that
/// (directly or indirectly) includes itself cannot recurse forever.
pub const MAX_NESTING: u32 = 8;

/// Most externals one top-level operation processes, across every nesting level, unless
/// [`crate::ClientConfig::max_externals`] says otherwise. Beyond it the remaining externals are
/// reported as failed, so a definition cannot make a checkout fetch without bound.
pub const MAX_EXTERNALS: usize = 1000;

/// True when `url_relpath` is `chain_relpath` or one of its ancestors: fetching it would fetch
/// the definition again (svn would recurse forever).
fn contains(url_relpath: &str, chain_relpath: &str) -> bool {
    url_relpath.is_empty()
        || chain_relpath == url_relpath
        || chain_relpath
            .strip_prefix(url_relpath)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// What processing externals produced: reports and the notifications of the drives it ran
/// (paths relative to the working copy root of the operation).
#[derive(Debug, Default)]
pub(crate) struct Outcome {
    pub reports: Vec<ExternalReport>,
    pub events: Vec<UpdateEvent>,
}

impl Outcome {
    /// Adds what an operation inside the external at `prefix` produced.
    fn absorb(&mut self, prefix: &RelPath, summary: UpdateSummary) -> Result<()> {
        for mut e in summary.events {
            e.path = wc::join_rel(prefix, &e.path)?;
            self.events.push(e);
        }
        for mut r in summary.externals {
            r.path = wc::join_rel(prefix, &r.path)?;
            self.reports.push(r);
        }
        Ok(())
    }
}

/// Runs `f` on the blocking pool.
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| SvnError::new(ErrorCode::Internal, format!("worker failed: {e}")))?
}

/// Holds the working copy lock on one directory (no levels below) and releases it on drop.
pub(crate) struct DirLock {
    db: Arc<WcDb>,
    dir: RelPath,
}

impl DirLock {
    /// Locks `dir` and finishes work an earlier operation left queued.
    pub(crate) fn obtain(db: &Arc<WcDb>, dir: &RelPath, cancel: &CancelToken) -> Result<Self> {
        db.wc_lock_obtain(dir, 0, ObtainMode::Cleanup)?;
        let lock = Self {
            db: Arc::clone(db),
            dir: dir.clone(),
        };
        wc::run_work_queue(db, cancel)?;
        Ok(lock)
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        if let Err(e) = self.db.wc_lock_release(&self.dir) {
            tracing::warn!("could not release working copy lock on '{}': {e}", self.dir);
        }
    }
}

fn report(
    path: &RelPath,
    kind: NodeKind,
    action: ExternalAction,
    revision: Option<svn_types::Revnum>,
) -> ExternalReport {
    ExternalReport {
        path: path.clone(),
        kind,
        action,
        revision,
        code: None,
        message: None,
    }
}

fn failed(path: &RelPath, kind: NodeKind, e: &SvnError) -> ExternalReport {
    ExternalReport {
        path: path.clone(),
        kind,
        action: ExternalAction::Failed,
        revision: None,
        code: Some(e.code),
        message: Some(e.message.clone()),
    }
}

/// Error for an external that cannot be placed where its definition puts it.
pub(crate) fn in_the_way(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::WcInvalidOperation, msg).with_svn_code(155_000)
}

fn path_key(p: &RelPath) -> String {
    if crate::status::CASE_INSENSITIVE_FS {
        p.as_str().to_lowercase()
    } else {
        p.as_str().to_owned()
    }
}

/// The defining directory of a definition: its URL, its repository root and its location as
/// `(repository UUID, repository path)`.
struct DefDir {
    url: RepoUrl,
    root: RepoUrl,
    location: (String, String),
}

/// URL of the versioned directory `dir`, its repository root and its location.
fn dir_urls(db: &WcDb, dir: &RelPath) -> Result<DefDir> {
    let info = db.read_repos_info(dir)?;
    let root = RepoUrl::parse(&info.repos_root_url)?;
    let url = if info.repos_relpath.is_empty() {
        root.clone()
    } else {
        root.join(&RelPath::new(&info.repos_relpath)?)?
    };
    Ok(DefDir {
        url,
        root,
        location: (info.repos_uuid, info.repos_relpath),
    })
}

/// Checks that the external at `local` (below the versioned directory `def_dir`) stays inside
/// the working copy: every existing directory from `def_dir` to the target's parent is a real
/// directory (not a symbolic link, not a file), and the target itself is not a symbolic link.
/// Returns the absolute path of the target.
pub(crate) fn checked_target(
    db: &WcDb,
    def_dir: &RelPath,
    local: &RelPath,
) -> Result<std::path::PathBuf> {
    let abs = wc::abs_path(db, local)?;
    let mut cur = wc::abs_path(db, def_dir)?;
    let rest = wc::strip_rel(local, def_dir)
        .ok_or_else(|| SvnError::new(ErrorCode::Internal, "external outside its definition"))?;
    let parts: Vec<&str> = rest.components().collect();
    for (i, part) in parts.iter().enumerate() {
        cur.push(part);
        let last = i + 1 == parts.len();
        match cur.symlink_metadata() {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(SvnError::new(
                    ErrorCode::UnsafePath,
                    format!(
                        "the external '{local}' would be written through the symbolic link '{}'",
                        cur.display()
                    ),
                ));
            }
            Ok(m) if !last && !m.is_dir() => {
                return Err(in_the_way(format!(
                    "cannot place the external '{local}': '{}' is not a directory",
                    cur.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => {
                return Err(SvnError::from_io(
                    format!("cannot stat '{}'", cur.display()),
                    &e,
                ));
            }
        }
    }
    Ok(abs)
}

/// Creates the missing directories between `def_dir` and the parent of the external at `abs`,
/// refusing symbolic links on the way (see [`checked_target`]).
pub(crate) fn create_parents(abs: &Path) -> Result<()> {
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SvnError::from_io(format!("cannot create '{}'", parent.display()), &e))?;
    }
    Ok(())
}

/// The context an operation inside the external at `prefix` runs with: one level deeper, below
/// the defining directory at `location`, sharing the operation's `budget`, with notifications
/// carrying the full path.
pub(crate) fn nested_ctx(
    ctx: &ClientCtx,
    prefix: &RelPath,
    location: &(String, String),
    budget: &Arc<AtomicUsize>,
) -> ClientCtx {
    let mut inner = ctx.clone();
    inner.external_nesting += 1;
    inner.external_chain.push(location.clone());
    inner.external_budget = Some(Arc::clone(budget));
    if let Some(p) = ctx.progress.clone() {
        let prefix = prefix.clone();
        inner.progress = Some(Arc::new(move |e: &UpdateEvent| {
            let mut e = e.clone();
            if let Ok(full) = wc::join_rel(&prefix, &e.path) {
                e.path = full;
            }
            p(&e);
        }));
    }
    inner
}

/// Processes the externals defined at or below `target` of the working copy `db` after a drive
/// with the requested `depth` (svn: only `Unknown` or `Infinity` process externals, and a
/// definition on a directory that is not at depth infinity only with `--depth infinity`).
///
/// Returns `Ok` with empty results when `ctx.externals` is `None`. Fails only for cancellation
/// and for working copy errors that stop the whole operation; everything else is a report.
///
/// The future is boxed: externals recurse (a directory external is checked out with its own
/// externals), and the type has to be named to be `Send`.
pub(crate) fn process<'a>(
    ctx: &'a ClientCtx,
    db: &'a Arc<WcDb>,
    target: &'a RelPath,
    depth: Depth,
    cancel: &'a CancelToken,
) -> Pin<Box<dyn Future<Output = Result<Outcome>> + Send + 'a>> {
    Box::pin(process_inner(ctx, db, target, depth, cancel))
}

async fn process_inner(
    ctx: &ClientCtx,
    db: &Arc<WcDb>,
    target: &RelPath,
    depth: Depth,
    cancel: &CancelToken,
) -> Result<Outcome> {
    let mut out = Outcome::default();
    let Some(opener) = ctx.externals.clone() else {
        return Ok(out);
    };
    if !matches!(depth, Depth::Unknown | Depth::Infinity) {
        return Ok(out);
    }
    let (defs, old) = {
        let (db, target) = (Arc::clone(db), target.clone());
        blocking(move || {
            Ok((
                db.externals_definitions_under(&target)?,
                db.externals_under(&RelPath::root())?,
            ))
        })
        .await?
    };
    // Recorded externals whose definition lives at or below the target, by path.
    let mut old: BTreeMap<String, External> = old
        .into_iter()
        .filter(|e| e.def_local_relpath.starts_with(target))
        .map(|e| (path_key(&e.local_relpath), e))
        .collect();
    let mut claimed = BTreeSet::new();
    let budget = ctx
        .external_budget
        .clone()
        .unwrap_or_else(|| Arc::new(AtomicUsize::new(0)));
    let max = ctx.config.max_externals.unwrap_or(MAX_EXTERNALS);
    for (def_dir, value, ambient) in defs {
        cancel.check()?;
        let keep_old = |old: &mut BTreeMap<String, External>| {
            old.retain(|_, e| e.def_local_relpath != def_dir);
        };
        if ambient != Depth::Infinity && depth != Depth::Infinity {
            keep_old(&mut old);
            continue;
        }
        let items = match parse_externals(def_dir.as_str(), &value) {
            Ok(items) => items,
            Err(e) => {
                keep_old(&mut old);
                out.reports.push(failed(&def_dir, NodeKind::Unknown, &e));
                continue;
            }
        };
        if ctx.external_nesting >= MAX_NESTING {
            keep_old(&mut old);
            let e = SvnError::new(
                ErrorCode::LimitExceeded,
                format!(
                    "externals nested more than {MAX_NESTING} levels deep are not fetched (a \
                     definition may include itself)"
                ),
            );
            out.reports.push(failed(&def_dir, NodeKind::Unknown, &e));
            continue;
        }
        let urls = {
            let (db, d) = (Arc::clone(db), def_dir.clone());
            blocking(move || dir_urls(&db, &d)).await
        };
        for item in items {
            cancel.check()?;
            let local = wc::join_rel(&def_dir, &item.target)?;
            let previous = old.remove(&path_key(&local));
            if !claimed.insert(path_key(&local)) {
                let e = in_the_way(format!(
                    "the external '{local}' is defined more than once; only the first \
                     definition is used"
                ));
                out.reports.push(failed(&local, NodeKind::Unknown, &e));
                continue;
            }
            if budget.fetch_add(1, Ordering::Relaxed) >= max {
                let e = SvnError::new(
                    ErrorCode::LimitExceeded,
                    format!(
                        "not fetched: this operation already processed {max} externals, the \
                         most it processes"
                    ),
                )
                .with_svn_code(205_011);
                out.reports.push(failed(&local, NodeKind::Unknown, &e));
                continue;
            }
            let res = match &urls {
                Ok(def) => {
                    let job = Item {
                        ctx,
                        opener: opener.as_ref(),
                        db,
                        def_dir: &def_dir,
                        local: &local,
                        item: &item,
                        previous: previous.as_ref(),
                        def_location: &def.location,
                        budget: &budget,
                        cancel,
                    };
                    job.run(&def.url, &def.root, &mut out).await
                }
                Err(e) => Err(e.clone()),
            };
            match res {
                Ok(()) => {}
                Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
                Err(e) => {
                    let kind = previous.as_ref().map_or(NodeKind::Unknown, |p| p.kind);
                    out.reports.push(failed(&local, kind, &e));
                }
            }
        }
    }
    // Recorded externals whose definition is gone.
    for (_, ext) in old {
        cancel.check()?;
        match remove::remove(ctx, db, &ext, cancel).await {
            Ok(r) => out.reports.push(r),
            Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
            Err(e) => out.reports.push(failed(&ext.local_relpath, ext.kind, &e)),
        }
    }
    Ok(out)
}

/// One definition being brought into the working copy.
pub(crate) struct Item<'a> {
    pub ctx: &'a ClientCtx,
    pub opener: &'a dyn SessionOpener,
    pub db: &'a Arc<WcDb>,
    pub def_dir: &'a RelPath,
    pub local: &'a RelPath,
    pub item: &'a ExternalItem,
    /// The `EXTERNALS` row recorded for this path before, if any.
    pub previous: Option<&'a External>,
    /// `(repository UUID, repository path)` of `def_dir`.
    pub def_location: &'a (String, String),
    /// Externals processed by the user's operation so far.
    pub budget: &'a Arc<AtomicUsize>,
    pub cancel: &'a CancelToken,
}

impl Item<'_> {
    /// Refuses an external whose URL is (or contains) its defining directory or a directory
    /// that led to it: fetching it would fetch the same definition again.
    fn refuse_cycle(&self, session: &dyn RaSession) -> Result<()> {
        let relpath = crate::update::repos_relpath_of(session.repos_root(), session.session_url())?;
        let uuid = session.uuid();
        let chain = self
            .ctx
            .external_chain
            .iter()
            .chain(std::iter::once(self.def_location));
        for (u, p) in chain {
            if u == uuid && contains(&relpath, p) {
                return Err(SvnError::new(
                    ErrorCode::LimitExceeded,
                    format!(
                        "not fetched: '{}' contains '{p}', whose externals led here, so it would \
                         include itself",
                        session.session_url()
                    ),
                )
                .with_svn_code(205_011));
            }
        }
        Ok(())
    }

    /// The context of an operation inside this external.
    pub(crate) fn nested_ctx(&self) -> ClientCtx {
        nested_ctx(self.ctx, self.local, self.def_location, self.budget)
    }

    async fn run(&self, dir_url: &RepoUrl, root: &RepoUrl, out: &mut Outcome) -> Result<()> {
        let url = resolve_url(&self.item.url, dir_url, root)?;
        {
            let (db, def, local) = (
                Arc::clone(self.db),
                self.def_dir.clone(),
                self.local.clone(),
            );
            blocking(move || {
                checked_target(&db, &def, &local)?;
                // The target must not be a versioned item of this working copy (a file
                // external of this definition is fine: it is updated).
                if let Some(n) = db.read_node(&local)?
                    && n.presence.is_present()
                    && !(n.op_depth == 0 && n.file_external)
                {
                    return Err(in_the_way(format!(
                        "cannot place the external '{local}': a versioned item of the working \
                         copy is in the way"
                    )));
                }
                Ok(())
            })
            .await?;
        }
        let session = self.opener.open(&url, self.cancel).await?;
        self.refuse_cycle(session.as_ref())?;
        let rev = match self.item.revision {
            ExtRev::Number(n) => svn_types::Revnum(n),
            ExtRev::Head => session.youngest_rev(self.cancel).await?,
        };
        let kind = session
            .check_path(&RelPath::root(), Some(rev), self.cancel)
            .await?;
        let kind = match kind {
            NodeKind::Dir | NodeKind::File => kind,
            _ => {
                return Err(SvnError::new(
                    ErrorCode::PathNotFound,
                    format!("URL '{url}' at revision {rev} doesn't exist"),
                )
                .with_svn_code(170_000));
            }
        };
        // A definition that now names the other kind replaces the old external first.
        if let Some(prev) = self.previous
            && prev.kind != kind
        {
            let r = remove::remove(self.ctx, self.db, prev, self.cancel).await?;
            if r.action != ExternalAction::Removed {
                return Err(in_the_way(format!(
                    "the external '{}' changed kind but the old one was kept: {}",
                    self.local,
                    r.message.unwrap_or_default()
                )));
            }
        }
        match kind {
            NodeKind::Dir => dir::ensure(self, &url, session, rev, out).await,
            _ => file::ensure(self, &url, rev, out).await,
        }
    }
}

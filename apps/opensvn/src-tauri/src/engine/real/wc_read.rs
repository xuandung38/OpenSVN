//! Reads of the working copy database: description of a working copy and repository location of
//! a node. Every call opens its own short-lived handle on a blocking thread; the app serializes
//! calls per working copy (`WcActor`), so no handle outlives a call.

use std::path::{Path, PathBuf};

use opensvn_ipc::{Rev, WcFormat};
use rusqlite::OptionalExtension;
use svn_types::{ErrorCode, RelPath, RepoUrl, SvnError};
use svn_wc::WcDb;
use svn_wc::db::{find_wcroot, sql_err};

use crate::engine::context::OpContext;
use crate::engine::facade::{EngineResult, WcInfo};

/// Runs blocking database work off the async executor.
pub(super) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> EngineResult<T> + Send + 'static,
) -> EngineResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| SvnError::new(ErrorCode::Internal, format!("database task failed: {e}")))?
}

/// Opens the working copy containing `path`.
pub(super) fn open_db(path: &Path) -> EngineResult<WcDb> {
    let root = find_wcroot(path)?;
    WcDb::open(&root)
}

/// Opens the working copy containing `path` under its canonical root path, the spelling
/// `svn_client::wc::open_containing` gives: commit journals are keyed by `WcDb::wcroot()`, so
/// commit and crash recovery must open a working copy the same way whatever path (symlink,
/// different case, `\\?\` prefix) the caller registered it under.
pub(super) fn open_db_canonical(path: &Path) -> EngineResult<WcDb> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|e| SvnError::from_io(format!("cannot resolve '{}'", path.display()), &e))?;
    open_db(&canonical)
}

fn revision(v: i64) -> EngineResult<u64> {
    u64::try_from(v).map_err(|_| SvnError::new(ErrorCode::Corrupt, "wc.db: negative revision"))
}

/// Describes the working copy behind `db` from its root BASE row.
pub(super) fn describe(db: &WcDb) -> EngineResult<WcInfo> {
    let wc_id = db.wc_id();
    let (repo_root, uuid, repos_path, base): (String, String, String, i64) = db.with_read(|c| {
        c.query_row(
            "SELECT r.root, r.uuid, n.repos_path, n.revision FROM nodes n \
             JOIN repository r ON r.id = n.repos_id \
             WHERE n.wc_id = ?1 AND n.local_relpath = '' AND n.op_depth = 0",
            [wc_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(sql_err)?
        .ok_or_else(|| SvnError::new(ErrorCode::Corrupt, "wc.db: working copy root has no BASE"))
    })?;
    let (lo, hi): (Option<i64>, Option<i64>) = db.with_read(|c| {
        c.query_row(
            "SELECT MIN(revision), MAX(revision) FROM nodes \
             WHERE wc_id = ?1 AND op_depth = 0 AND revision IS NOT NULL \
             AND presence IN ('normal', 'incomplete')",
            [wc_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(sql_err)
    })?;
    // `svn-client` marks a folder incomplete while a drive fills it and clears the mark when the
    // drive completes it, so any left over means a checkout or update stopped half-way.
    // svn records the revision a drive was reaching on every folder it marks incomplete
    // (`svn_wc__db_temp_op_start_directory_update`), so the highest one is where an
    // interrupted checkout or update was going.
    let (incomplete, resume): (bool, Option<i64>) = db.with_read(|c| {
        c.query_row(
            "SELECT COUNT(*) > 0, MAX(revision) FROM nodes \
             WHERE wc_id = ?1 AND op_depth = 0 AND presence = 'incomplete'",
            [wc_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(sql_err)
    })?;
    let rev_range = match (lo, hi) {
        (Some(lo), Some(hi)) if lo != hi => Some((Rev(revision(lo)?), Rev(revision(hi)?))),
        _ => None,
    };
    let url = RepoUrl::parse(&repo_root)?.join(&RelPath::new(&repos_path)?)?;
    Ok(WcInfo {
        root_path: db.wcroot().to_string_lossy().into_owned(),
        repo_root_url: repo_root,
        repo_uuid: uuid,
        url: url.to_string(),
        base_rev: Rev(revision(base)?),
        rev_range,
        format: if db.format() >= 32 {
            WcFormat::V32
        } else {
            WcFormat::V31
        },
        store_pristine: db.store_pristine(),
        path_in_repo: format!("/{repos_path}"),
        incomplete,
        resume_rev: resume.map(revision).transpose()?.map(Rev),
    })
}

/// Describes the working copy containing `root`.
pub(super) async fn wc_info(root: &Path, ctx: &OpContext) -> EngineResult<WcInfo> {
    ctx.check()?;
    let root: PathBuf = root.to_path_buf();
    blocking(move || describe(&open_db(&root)?)).await
}

/// Where a versioned node lives in the repository.
#[derive(Debug, Clone)]
pub(super) struct NodeLocation {
    /// `REPOSITORY.id` of the repository (key of the LOCK table).
    pub(super) repos_id: i64,
    /// Repository root URL.
    pub(super) repo_root: String,
    /// Path of the node below the repository root.
    pub(super) repos_path: RelPath,
    /// Revision of the node's BASE.
    pub(super) revision: u64,
}

/// Repository location of the BASE of `rel` (a path relative to the working copy `root`).
pub(super) async fn node_location(root: &Path, rel: &str) -> EngineResult<NodeLocation> {
    let root = root.to_path_buf();
    let rel = RelPath::new(rel)?;
    blocking(move || {
        let db = open_db(&root)?;
        let wc_id = db.wc_id();
        let row: Option<(i64, String, String, i64)> = db.with_read(|c| {
            c.query_row(
                "SELECT n.repos_id, r.root, n.repos_path, n.revision FROM nodes n \
                 JOIN repository r ON r.id = n.repos_id \
                 WHERE n.wc_id = ?1 AND n.local_relpath = ?2 AND n.op_depth = 0 \
                 AND n.presence IN ('normal', 'incomplete')",
                (wc_id, rel.as_str()),
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(sql_err)
        })?;
        let (repos_id, repo_root, repos_path, base) = row.ok_or_else(|| {
            SvnError::new(
                ErrorCode::PathNotFound,
                format!("'{rel}' is not in the repository (not versioned or only scheduled)"),
            )
        })?;
        Ok(NodeLocation {
            repos_id,
            repo_root,
            repos_path: RelPath::new(&repos_path)?,
            revision: revision(base)?,
        })
    })
    .await
}

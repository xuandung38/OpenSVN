//! Writing the two sides of a diff to files (`export_wc_sides`, `export_rev_sides`), for viewers
//! that need whole files rather than inline text: the Office text diff, opening a version in its
//! default application and external diff tools.
//!
//! Sides are streamed: a pristine or working file is copied, a repository file is downloaded
//! straight into its destination, so size does not matter. The working side is the file exactly
//! as on disk (a symlink as its `link <target>` text); the other sides are repository-normal.
//! The node is chosen like `diff_wc` chooses it (copy source, BASE, unversioned file) and the
//! same path checks apply to unversioned files.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use opensvn_ipc::Rev;
use svn_types::{ErrorCode, NodeKind, RelPath, Revnum, SvnError};
use svn_wc::db::{Node, Presence, WcDb};
use svn_wc::pristine::Sha1Digest;
use svn_wc::translate::special;
use tokio::io::AsyncWriteExt;

use super::wc_status::{file_on_disk, not_versioned, open_seen_file, unversioned_path};
use super::{RealEngine, wc_read};
use crate::engine::context::OpContext;
use crate::engine::facade::{EngineResult, ExportedSides, SideFiles, WcRef};
use crate::engine::mapping::revnum;

fn create(dest: &Path) -> EngineResult<File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| SvnError::from_io(format!("cannot create {}", dest.display()), &e))
}

fn copy_into(mut src: impl std::io::Read, dest: &Path) -> EngineResult<()> {
    let mut out = create(dest)?;
    std::io::copy(&mut src, &mut out)
        .map_err(|e| SvnError::from_io(format!("cannot write {}", dest.display()), &e))?;
    Ok(())
}

fn write_bytes(data: &[u8], dest: &Path) -> EngineResult<()> {
    copy_into(data, dest)
}

fn directory(rel: &RelPath) -> SvnError {
    SvnError::new(
        ErrorCode::WcInvalidOperation,
        format!("'{rel}' is a directory; only files can be opened"),
    )
}

/// Copies the pristine text of `node` (empty when it has none).
fn export_pristine(db: &WcDb, node: &Node, dest: &Path) -> EngineResult<()> {
    match node
        .checksum
        .as_deref()
        .and_then(Sha1Digest::from_svn_string)
    {
        Some(sha1) => copy_into(db.pristine_store().open_text(&sha1)?, dest),
        None => write_bytes(&[], dest),
    }
}

/// Copies the file at `abs` as it is on disk; false when there is none.
fn export_on_disk(abs: &Path, rel: &RelPath, dest: &Path) -> EngineResult<bool> {
    let Some(seen) = file_on_disk(abs, rel)? else {
        return Ok(false);
    };
    if seen.file_type().is_symlink() {
        write_bytes(&special::read_repo_normal(abs)?, dest)?;
    } else {
        copy_into(open_seen_file(abs, &seen, rel)?, dest)?;
    }
    Ok(true)
}

/// What the local part of a working copy export found.
struct LocalExport {
    base: bool,
    working: bool,
    repo_root: String,
    repos_path: String,
    unversioned: bool,
}

fn export_unversioned(
    db: &WcDb,
    rel: &RelPath,
    working_to: Option<&Path>,
) -> EngineResult<LocalExport> {
    let abs = unversioned_path(db.wcroot(), rel)?;
    match fs::symlink_metadata(&abs) {
        Ok(md) if md.file_type().is_dir() => return Err(directory(rel)),
        Ok(md) if md.file_type().is_file() || md.file_type().is_symlink() => {}
        Ok(_) => return Err(not_versioned(rel)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(not_versioned(rel)),
        Err(e) => return Err(SvnError::from_io(format!("cannot stat '{rel}'"), &e)),
    }
    let working = match working_to {
        Some(dest) => export_on_disk(&abs, rel, dest)?,
        None => false,
    };
    Ok(LocalExport {
        base: false,
        working,
        repo_root: String::new(),
        repos_path: String::new(),
        unversioned: true,
    })
}

/// Writes the pristine side (when `base_to` is set) and the working file (when `working_to` is
/// set) of `rel`, and finds where the node lives in the repository.
fn export_local(
    root: &Path,
    rel: &RelPath,
    base_to: Option<&Path>,
    working_to: Option<&Path>,
) -> EngineResult<LocalExport> {
    let db = wc_read::open_db(root)?;
    let cur = match db.read_node(rel)? {
        Some(n) if n.presence == Presence::BaseDeleted || n.presence.is_present() => n,
        _ => return export_unversioned(&db, rel, working_to),
    };
    if !matches!(cur.kind, NodeKind::File | NodeKind::Symlink) {
        return Err(directory(rel));
    }
    let deleted = cur.presence == Presence::BaseDeleted;
    let base = db.read_base(rel)?.filter(|b| b.presence.is_present());
    let copy_source = (cur.op_depth > 0 && !deleted && cur.revision.is_some()).then_some(&cur);
    let old = copy_source.or(base.as_ref());
    let base_written = match (old, base_to) {
        (Some(node), Some(dest)) => {
            export_pristine(&db, node, dest)?;
            true
        }
        _ => false,
    };
    let working = match working_to {
        Some(dest) if !deleted => {
            let abs = svn_client::wc::abs_path(&db, rel)?;
            export_on_disk(&abs, rel, dest)?
        }
        _ => false,
    };
    let (repo_root, _) = svn_client::wc::repos_identity(&db)?;
    let repos_path = match base.as_ref().and_then(|b| b.repos_relpath.clone()) {
        Some(p) => p,
        None => {
            let root_path = db
                .read_base(&RelPath::root())?
                .and_then(|n| n.repos_relpath)
                .unwrap_or_default();
            svn_client::wc::join_rel(&RelPath::new(&root_path)?, rel)?
                .as_str()
                .to_owned()
        }
    };
    Ok(LocalExport {
        base: base_written,
        working,
        repo_root,
        repos_path,
        unversioned: false,
    })
}

impl RealEngine {
    /// Downloads `path@rev` of the session into `dest`; false (and no file) when the file does
    /// not exist at that revision.
    async fn download_side(
        &self,
        session: &dyn svn_ra_api::RaSession,
        path: &RelPath,
        rev: u64,
        dest: &Path,
        ctx: &OpContext,
    ) -> EngineResult<bool> {
        if rev == 0 {
            return Ok(false);
        }
        let io = |what: &str, e: &std::io::Error| {
            SvnError::from_io(format!("cannot {what} {}", dest.display()), e)
        };
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)
            .await
            .map_err(|e| io("create", &e))?;
        let got = session
            .get_file(path, Some(Revnum(rev)), &mut file, &ctx.cancel)
            .await;
        match got {
            Ok(_) => {
                file.flush().await.map_err(|e| io("write", &e))?;
                Ok(true)
            }
            Err(e) if e.code == ErrorCode::PathNotFound => {
                drop(file);
                let _ = tokio::fs::remove_file(dest).await;
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    pub(super) async fn do_export_wc_sides(
        &self,
        wc: &WcRef,
        path: &str,
        against: Option<Rev>,
        to: SideFiles<'_>,
        ctx: &OpContext,
    ) -> EngineResult<ExportedSides> {
        ctx.check()?;
        let rel = RelPath::new(path)?;
        let shown = rel.as_str().to_owned();
        let owned = |p: Option<&Path>| p.map(Path::to_path_buf);
        let base_to: Option<PathBuf> = if against.is_none() {
            owned(to.left)
        } else {
            None
        };
        let working_to = owned(to.right);
        let local = {
            let (root, rel) = (wc.root.clone(), rel.clone());
            wc_read::blocking(move || {
                export_local(&root, &rel, base_to.as_deref(), working_to.as_deref())
            })
            .await?
        };
        if local.unversioned && against.is_some() {
            return Err(not_versioned(&rel));
        }
        let left = match (against, to.left) {
            (None, _) => local.base.then(|| format!("{shown} (base)")),
            (Some(r), Some(dest)) if !local.unversioned => {
                let (session, path) = self
                    .locate_repo_path(&local.repo_root, &RelPath::new(&local.repos_path)?, ctx)
                    .await?;
                self.download_side(session.as_ref(), &path, r.0, dest, ctx)
                    .await?
                    .then(|| format!("{shown}@r{}", r.0))
            }
            _ => None,
        };
        Ok(ExportedSides {
            left,
            right: local.working.then(|| format!("{shown} (working)")),
        })
    }

    pub(super) async fn do_export_rev_sides(
        &self,
        url: &str,
        from: Option<Rev>,
        to_rev: Rev,
        to: SideFiles<'_>,
        ctx: &OpContext,
    ) -> EngineResult<ExportedSides> {
        let (session, path) = self.locate(url, ctx).await?;
        let from = revnum(from).map_or(to_rev.0.saturating_sub(1), |r| r.0);
        let name = path.file_name().unwrap_or("").to_owned();
        let mut out = ExportedSides::default();
        if let Some(dest) = to.left
            && self
                .download_side(session.as_ref(), &path, from, dest, ctx)
                .await?
        {
            out.left = Some(format!("{name}@r{from}"));
        }
        if let Some(dest) = to.right
            && self
                .download_side(session.as_ref(), &path, to_rev.0, dest, ctx)
                .await?
        {
            out.right = Some(format!("{name}@r{}", to_rev.0));
        }
        Ok(out)
    }
}

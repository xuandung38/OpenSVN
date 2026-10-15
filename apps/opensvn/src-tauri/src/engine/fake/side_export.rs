//! `export_wc_sides` / `export_rev_sides` of the fake engine: the sides its diffs compare,
//! written to files.

use std::path::Path;

use opensvn_ipc::Rev;
use svn_ra_api::RaSession;
use svn_types::{ErrorCode, RelPath, Revnum, SvnError};

use super::FakeEngine;
use super::state::rel;
use crate::engine::context::OpContext;
use crate::engine::facade::{ExportedSides, SideFiles, WcRef};

fn write_new(dest: &Path, data: &[u8]) -> Result<(), SvnError> {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .and_then(|mut f| f.write_all(data))
        .map_err(|e| SvnError::from_io(format!("cannot write {}", dest.display()), &e))
}

impl FakeEngine {
    /// Writes `path@rev` of `session` to `dest`; false when it does not exist there.
    async fn export_repo_side(
        &self,
        session: &svn_ra_api::fake::FakeRaSession,
        path: &RelPath,
        rev: u64,
        dest: &Path,
        ctx: &OpContext,
    ) -> Result<bool, SvnError> {
        if rev == 0 {
            return Ok(false);
        }
        let mut data = Vec::new();
        match session
            .get_file(path, Some(Revnum(rev)), &mut data, &ctx.cancel)
            .await
        {
            Ok(_) => write_new(dest, &data).map(|()| true),
            Err(e) if e.code == ErrorCode::PathNotFound => Ok(false),
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
    ) -> Result<ExportedSides, SvnError> {
        let rp = rel(path)?;
        let (node, repo_root, repo_path) = self.with_wc(&wc.root, |w| {
            Ok((
                w.node(&rp)?.clone(),
                w.repo_root_url.clone(),
                w.repo_path(&rp),
            ))
        })?;
        if node.kind == opensvn_ipc::FileKind::Dir {
            return Err(SvnError::new(
                ErrorCode::WcInvalidOperation,
                format!("'{rp}' is a directory; only files can be opened"),
            ));
        }
        let mut out = ExportedSides::default();
        if let Some(dest) = to.left {
            out.left = match against {
                None => match node.base.as_ref() {
                    Some(b) => {
                        write_new(dest, b)?;
                        Some(format!("{rp} (base)"))
                    }
                    None => None,
                },
                Some(r) => {
                    let session = self.repo_by_root(&repo_root)?;
                    let path = RelPath::new(repo_path.trim_matches('/'))?;
                    self.export_repo_side(&session, &path, r.0, dest, ctx)
                        .await?
                        .then(|| format!("{rp}@r{}", r.0))
                }
            };
        }
        if let (Some(dest), Some(work)) = (to.right, node.work.as_ref()) {
            write_new(dest, work)?;
            out.right = Some(format!("{rp} (working)"));
        }
        Ok(out)
    }

    pub(super) async fn do_export_rev_sides(
        &self,
        url: &str,
        from: Option<Rev>,
        to_rev: Rev,
        to: SideFiles<'_>,
        ctx: &OpContext,
    ) -> Result<ExportedSides, SvnError> {
        let (session, path) = self.locate(url)?;
        let from = from.map_or(to_rev.0.saturating_sub(1), |r| r.0);
        let name = path.file_name().unwrap_or("").to_owned();
        let mut out = ExportedSides::default();
        if let Some(dest) = to.left
            && self
                .export_repo_side(&session, &path, from, dest, ctx)
                .await?
        {
            out.left = Some(format!("{name}@r{from}"));
        }
        if let Some(dest) = to.right
            && self
                .export_repo_side(&session, &path, to_rev.0, dest, ctx)
                .await?
        {
            out.right = Some(format!("{name}@r{}", to_rev.0));
        }
        Ok(out)
    }
}

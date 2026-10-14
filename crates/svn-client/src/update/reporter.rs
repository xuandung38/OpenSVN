//! Builds the working copy report (`set_path` / `delete_path` / `link_path`) that precedes an
//! update, switch or status drive.

use std::sync::Arc;

use svn_ra_api::ReportFn;
use svn_types::{Depth, ErrorCode, NodeKind, RelPath, RepoUrl, Reporter, Result, Revnum, SvnError};
use svn_wc::db::{Node, Presence, WcDb};

use super::editor::repos_child;
use crate::wc::join_rel;

fn lock_token(db: &WcDb, wc: &RelPath) -> Result<Option<String>> {
    Ok(db.repos_lock(wc)?.map(|l| l.token))
}

struct Ctx<'a> {
    db: &'a WcDb,
    anchor: &'a RelPath,
    root_url: RepoUrl,
    /// `--set-depth`: excluded nodes are reported as missing so the server sends them again
    /// when the new depth covers them (svn's `honor_depth_exclude = FALSE`).
    sticky: bool,
}

impl Ctx<'_> {
    fn url_of(&self, repos_path: &str) -> Result<RepoUrl> {
        self.root_url.join(&RelPath::new(repos_path)?)
    }

    /// Reports `node` at `report_path` (anchor relative); `parent_repos` is where the parent
    /// expects the child, which decides between `set_path` and `link_path`.
    #[allow(clippy::too_many_arguments)]
    fn report_entry(
        &self,
        r: &mut dyn Reporter,
        report_path: &RelPath,
        wc: &RelPath,
        node: &Node,
        parent_repos: Option<&str>,
        depth: Depth,
        force: bool,
        parent_rev: Option<u64>,
    ) -> Result<bool> {
        let rev = Revnum(node.revision.unwrap_or(0));
        let start_empty = node.presence == Presence::Incomplete;
        let token = if node.kind == NodeKind::File {
            lock_token(self.db, wc)?
        } else {
            None
        };
        let repos = node.repos_relpath.clone().unwrap_or_default();
        let name = wc.file_name().unwrap_or("");
        let switched = parent_repos.is_some_and(|p| repos_child(p, name) != repos);
        let rev_differs = parent_rev != Some(rev.0);
        let non_default_depth = node.kind == NodeKind::Dir && depth != Depth::Infinity;
        if !(force
            || switched
            || rev_differs
            || start_empty
            || token.is_some()
            || non_default_depth)
        {
            return Ok(false);
        }
        if switched {
            r.link_path(
                report_path,
                &self.url_of(&repos)?,
                rev,
                depth,
                start_empty,
                token.as_deref(),
            )?;
        } else {
            r.set_path(report_path, rev, depth, start_empty, token.as_deref())?;
        }
        Ok(true)
    }

    fn report_dir(
        &self,
        r: &mut dyn Reporter,
        wc: &RelPath,
        report_path: &RelPath,
        dir: &Node,
    ) -> Result<()> {
        let dir_repos = dir.repos_relpath.clone().unwrap_or_default();
        for child in self.db.children(wc)? {
            let Some(node) = self.db.read_base(&child)? else {
                continue;
            };
            let name = child.file_name().unwrap_or("");
            let child_report = join_rel(report_path, &RelPath::new(name)?)?;
            match node.presence {
                Presence::NotPresent | Presence::ServerExcluded => {
                    r.delete_path(&child_report)?;
                    continue;
                }
                Presence::Excluded if self.sticky => {
                    r.delete_path(&child_report)?;
                    continue;
                }
                Presence::Excluded => {
                    r.set_path(
                        &child_report,
                        Revnum(node.revision.unwrap_or(0)),
                        Depth::Exclude,
                        false,
                        None,
                    )?;
                    continue;
                }
                Presence::BaseDeleted => continue,
                Presence::Normal | Presence::Incomplete => {}
            }
            let depth = match node.kind {
                NodeKind::Dir => node
                    .depth
                    .filter(|d| *d != Depth::Unknown)
                    .unwrap_or(Depth::Infinity),
                _ => Depth::Infinity,
            };
            self.report_entry(
                r,
                &child_report,
                &child,
                &node,
                Some(&dir_repos),
                depth,
                false,
                dir.revision,
            )?;
            if node.kind == NodeKind::Dir {
                self.report_dir(r, &child, &child_report, &node)?;
            }
        }
        Ok(())
    }
}

/// Describes the working copy state below `target` (anchor relative) to the server.
///
/// `depth` overrides the recorded depth of the target when it is not `Unknown`. With `sticky`
/// (`--set-depth`) the target is reported at its recorded depth whatever `depth` says, so the
/// server sends what a deeper requested depth adds, and excluded nodes are reported missing.
pub fn build_report(
    db: Arc<WcDb>,
    anchor: RelPath,
    target: RelPath,
    depth: Depth,
    sticky: bool,
    repos_root: String,
) -> ReportFn {
    Box::new(move |r: &mut dyn Reporter| {
        let target_wc = join_rel(&anchor, &target)?;
        let node = db.read_base(&target_wc)?.ok_or_else(|| {
            SvnError::new(
                ErrorCode::PathNotFound,
                format!("'{target_wc}' is not under version control"),
            )
        })?;
        let ctx = Ctx {
            db: &db,
            anchor: &anchor,
            root_url: RepoUrl::parse(&repos_root)?,
            sticky,
        };
        let _ = ctx.anchor;
        let recorded = if node.kind == NodeKind::Dir {
            node.depth
                .filter(|d| *d != Depth::Unknown)
                .unwrap_or(Depth::Infinity)
        } else {
            Depth::Infinity
        };
        // Like svn's crawler: a narrower requested depth is reported as the target's depth
        // (`honor_depth_exclude`), anything else reports what the working copy has.
        let narrower = |d: Depth| match d {
            Depth::Empty => 0,
            Depth::Files => 1,
            Depth::Immediates => 2,
            _ => 3,
        };
        let report_depth = if !sticky
            && !matches!(depth, Depth::Unknown | Depth::Exclude)
            && narrower(depth) < narrower(recorded)
        {
            depth
        } else {
            recorded
        };
        let parent_repos = match target_wc.parent() {
            Some(p) => db.read_base(&p)?.and_then(|n| n.repos_relpath),
            None => None,
        };
        ctx.report_entry(
            r,
            &target,
            &target_wc,
            &node,
            parent_repos.as_deref(),
            report_depth,
            true,
            None,
        )?;
        if node.kind == NodeKind::Dir {
            ctx.report_dir(r, &target_wc, &target, &node)?;
        }
        r.finish()
    })
}

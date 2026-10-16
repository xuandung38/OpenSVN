//! Builds the working copy report (`set_path` / `delete_path` / `link_path`) that precedes an
//! update, switch or status drive.

use std::collections::HashMap;
use std::sync::Arc;

use svn_ra_api::ReportFn;
use svn_types::{Depth, ErrorCode, NodeKind, RelPath, RepoUrl, Reporter, Result, Revnum, SvnError};
use svn_wc::db::{Node, Presence, WcDb};

use super::editor::repos_child;
use crate::wc::join_rel;

struct Ctx<'a> {
    db: &'a WcDb,
    /// Lock tokens of the BASE nodes of the reported tree, read once (one query instead of one
    /// per file).
    locks: HashMap<RelPath, String>,
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
            self.locks.get(wc).cloned()
        } else {
            None
        };
        let repos = node.repos_relpath.clone().unwrap_or_default();
        let name = wc.file_name().unwrap_or("");
        let switched = parent_repos.is_some_and(|p| repos_child(p, name) != repos);
        let rev_differs = parent_rev != Some(rev.0);
        if !(force || switched || rev_differs || start_empty || token.is_some()) {
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

    /// Reports the BASE children of `wc` like svn's `report_revisions_and_depths`: `dir_depth`
    /// is the depth the directory was reported with, `depth` the depth of the crawl (the
    /// requested depth, `Unknown`: everything the working copy has).
    fn report_dir(
        &self,
        r: &mut dyn Reporter,
        wc: &RelPath,
        report_path: &RelPath,
        dir: &Node,
        dir_depth: Depth,
        depth: Depth,
    ) -> Result<()> {
        let recursive = matches!(depth, Depth::Infinity | Depth::Unknown);
        let dir_repos = dir.repos_relpath.clone().unwrap_or_default();
        // An `incomplete` directory is reported `start_empty`, so the server assumes it has no
        // children: every child must be reported, even one at the directory's revision
        // (libsvn_wc's `report_everything`). Otherwise the server never learns about a child
        // directory at that revision while its own deeper entries are reported below it, and
        // mod_dav_svn rejects the report (#119).
        let report_everything = dir.presence == Presence::Incomplete;
        // One query per directory, like svn's crawler (`svn_wc__db_base_get_children_info`).
        for node in self.db.base_children(wc)? {
            let child = node.relpath.clone();
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
            // File externals are not part of this tree; their own fetch reports them.
            if node.file_external {
                continue;
            }
            if node.kind != NodeKind::Dir {
                // The mere presence of a file in a depth-empty directory is news to the server.
                self.report_entry(
                    r,
                    &child_report,
                    &child,
                    &node,
                    Some(&dir_repos),
                    Depth::Infinity,
                    report_everything || dir_depth == Depth::Empty,
                    dir.revision,
                )?;
                continue;
            }
            if !(recursive || depth == Depth::Immediates) {
                continue;
            }
            let child_depth = node
                .depth
                .filter(|d| *d != Depth::Unknown)
                .unwrap_or(Depth::Infinity);
            let report_depth = if recursive { child_depth } else { Depth::Empty };
            let force = report_everything
                || matches!(dir_depth, Depth::Empty | Depth::Files)
                || (dir_depth == Depth::Immediates && child_depth != Depth::Empty)
                || (child_depth != Depth::Infinity && recursive);
            self.report_entry(
                r,
                &child_report,
                &child,
                &node,
                Some(&dir_repos),
                report_depth,
                force,
                dir.revision,
            )?;
            if recursive {
                self.report_dir(r, &child, &child_report, &node, child_depth, depth)?;
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
        let node = db.read_base(&target_wc)?;
        // `--set-depth` on a node the working copy does not have (outside its parent's depth,
        // `not-present` or excluded): reported missing so the server adds it, like svn's
        // crawler (`set_path("", 0, depth)` then `delete_path("")`).
        if sticky
            && node
                .as_ref()
                .is_none_or(|n| !n.presence.is_present() && n.presence != Presence::BaseDeleted)
        {
            let depth = match depth {
                Depth::Unknown | Depth::Exclude => Depth::Infinity,
                d => d,
            };
            r.set_path(&target, Revnum(0), depth, false, None)?;
            r.delete_path(&target)?;
            return r.finish();
        }
        let node = node.ok_or_else(|| {
            SvnError::new(
                ErrorCode::PathNotFound,
                format!("'{target_wc}' is not under version control"),
            )
        })?;
        let locks = db
            .repos_locks_below(&target_wc)?
            .into_iter()
            .map(|(path, _, lock)| (path, lock.token))
            .collect();
        let ctx = Ctx {
            db: &db,
            locks,
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
        // The anchor itself (`target` is the root of the drive) is where the session URL says
        // it is, switched or not: a plain `set_path("")`, like svn's crawler. A directory
        // switched away from its parent is always its own anchor (`drive_update`).
        let parent = match target_wc.parent().filter(|_| !target.is_root()) {
            Some(p) => db.read_base(&p)?,
            None => None,
        };
        let parent_repos = parent.as_ref().and_then(|n| n.repos_relpath.clone());
        let switched = parent_repos.as_deref().is_some_and(|p| {
            repos_child(p, target_wc.file_name().unwrap_or(""))
                != node.repos_relpath.clone().unwrap_or_default()
        });
        if switched && node.kind != NodeKind::Dir {
            // A switched file target is described by a `link_path`, which may not open the
            // report: svn_repos takes a first `set_path` of the operand as a place-holder
            // carrying the anchor's revision and lets the next entry describe it
            // (`svn_wc_crawl_revisions5` reports a switched file this way; reporter.c
            // `finish_report`). A file has no entries below, so nothing else follows.
            let rev = parent
                .as_ref()
                .and_then(|n| n.revision)
                .or(node.revision)
                .unwrap_or(0);
            r.set_path(&target, Revnum(rev), report_depth, false, None)?;
        } else if switched {
            return Err(SvnError::new(
                ErrorCode::Internal,
                format!(
                    "'{target_wc}' is switched: its update must be anchored at it, not reported \
                     with a link below its parent"
                ),
            ));
        }
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
        if node.kind == NodeKind::Dir && depth != Depth::Empty {
            ctx.report_dir(r, &target_wc, &target, &node, report_depth, depth)?;
        }
        r.finish()
    })
}

//! Branch tree of a repository for a layout (trunk/branches/tags patterns).

use opensvn_ipc::{BranchKind, BranchNode, Layout};
use svn_types::{Depth, NodeKind, RelPath};

use super::RealEngine;
use crate::engine::context::OpContext;
use crate::engine::facade::EngineResult;

/// Placeholder for the project directory in custom layout patterns.
const PROJECT: &str = "{project}";

impl RealEngine {
    pub(super) async fn do_branches_list(
        &self,
        repo_root_url: &str,
        layout: &Layout,
        ctx: &OpContext,
    ) -> EngineResult<Vec<BranchNode>> {
        let (trunks, branches, tags): (Vec<String>, Vec<String>, Vec<String>) = match layout {
            Layout::None => return Ok(Vec::new()),
            Layout::Auto => (
                vec!["/trunk".into()],
                vec!["/branches".into()],
                vec!["/tags".into()],
            ),
            Layout::Custom {
                trunk,
                branches,
                tags,
            } => (vec![trunk.clone()], branches.clone(), tags.clone()),
        };
        let (session, root) = self.locate(repo_root_url, ctx).await?;
        let per_project = [&trunks, &branches, &tags]
            .iter()
            .any(|v| v.iter().any(|p| p.contains(PROJECT)));
        let projects: Vec<String> = if per_project {
            session
                .list(&root, None, Depth::Immediates, &ctx.cancel)
                .await?
                .into_iter()
                .filter(|e| e.kind == NodeKind::Dir)
                .map(|e| e.name)
                .collect()
        } else {
            vec![String::new()]
        };
        let base = repo_root_url.trim_end_matches('/');
        let mut out = Vec::new();
        for project in &projects {
            let expand = |p: &str| p.replace(PROJECT, project).trim_matches('/').to_owned();
            for t in &trunks {
                let path = expand(t);
                if let Ok(rp) = RelPath::new(&path)
                    && session.check_path(&rp, None, &ctx.cancel).await? == NodeKind::Dir
                {
                    out.push(BranchNode {
                        kind: BranchKind::Trunk,
                        name: "trunk".into(),
                        url: format!("{base}/{path}"),
                    });
                }
            }
            for (patterns, kind) in [(&branches, BranchKind::Branch), (&tags, BranchKind::Tag)] {
                for pattern in patterns {
                    let path = expand(pattern);
                    let Ok(rp) = RelPath::new(&path) else {
                        continue;
                    };
                    if session.check_path(&rp, None, &ctx.cancel).await? != NodeKind::Dir {
                        continue;
                    }
                    for e in session
                        .list(&rp, None, Depth::Immediates, &ctx.cancel)
                        .await?
                    {
                        if e.kind == NodeKind::Dir {
                            out.push(BranchNode {
                                kind,
                                url: format!("{base}/{path}/{}", e.name),
                                name: e.name,
                            });
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

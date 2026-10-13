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
                .filter(|e| e.kind == NodeKind::Dir && server_name_ok(&e.name))
                .map(|e| e.name)
                .collect()
        } else {
            vec![String::new()]
        };
        let repos_root = session.repos_root().clone();
        let mut out = Vec::new();
        for project in &projects {
            let expand = |p: &str| p.replace(PROJECT, project).trim_matches('/').to_owned();
            for t in &trunks {
                let Ok(rp) = RelPath::new(&expand(t)) else {
                    continue;
                };
                if session.check_path(&rp, None, &ctx.cancel).await? == NodeKind::Dir {
                    out.push(BranchNode {
                        kind: BranchKind::Trunk,
                        name: "trunk".into(),
                        url: repos_root.join(&rp)?.to_string(),
                    });
                }
            }
            for (patterns, kind) in [(&branches, BranchKind::Branch), (&tags, BranchKind::Tag)] {
                for pattern in patterns {
                    let Ok(rp) = RelPath::new(&expand(pattern)) else {
                        continue;
                    };
                    if session.check_path(&rp, None, &ctx.cancel).await? != NodeKind::Dir {
                        continue;
                    }
                    for e in session
                        .list(&rp, None, Depth::Immediates, &ctx.cancel)
                        .await?
                    {
                        if e.kind != NodeKind::Dir {
                            continue;
                        }
                        // Server names are untrusted: validate them and let the URL encode them.
                        let Some(path) = server_child(&rp, &e.name) else {
                            continue;
                        };
                        out.push(BranchNode {
                            kind,
                            url: repos_root.join(&path)?.to_string(),
                            name: e.name,
                        });
                    }
                }
            }
        }
        Ok(out)
    }
}

/// Whether a directory name listed by the server is safe to use as a path segment.
fn server_name_ok(name: &str) -> bool {
    let ok = !name.contains('/') && RelPath::from_server(name).is_ok();
    if !ok {
        tracing::warn!(name, "skipping a repository directory with an unsafe name");
    }
    ok
}

/// `parent/name` for a directory name listed by the server, `None` when the name is unsafe.
fn server_child(parent: &RelPath, name: &str) -> Option<RelPath> {
    if !server_name_ok(name) {
        return None;
    }
    let child = RelPath::from_server(name).ok()?;
    parent.child(child.as_str()).ok()
}

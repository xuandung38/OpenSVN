//! Compare with branch / another folder for the fake engine: both sides are read in full and
//! compared node by node (kind, text, properties). Same answers as `svn diff --summarize` with
//! ancestry ignored, which is what the real engine asks the server for.

use std::collections::BTreeMap;

use bytes::Bytes;
use opensvn_ipc::{CompareChange, CompareEntry, CompareSummary, EntryKind, FileKind, Rev};
use svn_ra_api::RaSession;
use svn_ra_api::fake::FakeRaSession;
use svn_types::{CancelToken, Depth, ErrorCode, NodeKind, RelPath, RepoUrl, Revnum, SvnError};

use super::FakeEngine;
use super::state::Sched;
use crate::engine::context::OpContext;
use crate::engine::facade::CompareLeft;

/// One node of a side: `None` content for a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Side {
    content: Option<Bytes>,
    props: BTreeMap<String, Bytes>,
}

type Tree = BTreeMap<RelPath, Side>;

fn join(dir: &RelPath, name: &str) -> Result<RelPath, SvnError> {
    if dir.is_root() {
        RelPath::new(name)
    } else {
        RelPath::new(&format!("{dir}/{name}"))
    }
}

/// Everything at and below `path` of `session` at `rev`, keyed relative to `path`.
async fn read_tree(
    session: &FakeRaSession,
    path: &RelPath,
    rev: Revnum,
    cancel: &CancelToken,
) -> Result<Tree, SvnError> {
    let mut tree = Tree::new();
    tree.insert(
        RelPath::root(),
        Side {
            content: None,
            props: BTreeMap::new(),
        },
    );
    let mut todo = vec![RelPath::root()];
    while let Some(rel) = todo.pop() {
        cancel.check()?;
        let at = if rel.is_root() {
            path.clone()
        } else {
            join(path, rel.as_str())?
        };
        for e in session
            .list(&at, Some(rev), Depth::Immediates, cancel)
            .await?
        {
            let child = join(&rel, &e.name)?;
            if e.kind == NodeKind::Dir {
                todo.push(child.clone());
                tree.insert(
                    child,
                    Side {
                        content: None,
                        props: BTreeMap::new(),
                    },
                );
            } else {
                let mut buf = Vec::new();
                let meta = session
                    .get_file(&join(&at, &e.name)?, Some(rev), &mut buf, cancel)
                    .await?;
                let props = meta
                    .props
                    .into_iter()
                    .filter(|p| !p.name.starts_with("svn:entry:"))
                    .filter_map(|p| p.value.map(|v| (p.name, v)))
                    .collect();
                tree.insert(
                    child,
                    Side {
                        content: Some(Bytes::from(buf)),
                        props,
                    },
                );
            }
        }
    }
    Ok(tree)
}

fn kind_of(s: &Side) -> EntryKind {
    if s.content.is_some() {
        EntryKind::File
    } else {
        EntryKind::Dir
    }
}

/// The differences from `left` to `right`, sorted by path.
fn summarize(left: &Tree, right: &Tree) -> Vec<CompareEntry> {
    let mut out = Vec::new();
    let entry = |path: &RelPath, s: &Side, change, props_changed| CompareEntry {
        path: path.as_str().to_owned(),
        kind: kind_of(s),
        change,
        props_changed,
    };
    let paths: std::collections::BTreeSet<&RelPath> = left.keys().chain(right.keys()).collect();
    for path in paths {
        match (left.get(path), right.get(path)) {
            (Some(l), None) => out.push(entry(path, l, CompareChange::Deleted, false)),
            (None, Some(r)) => out.push(entry(path, r, CompareChange::Added, false)),
            (Some(l), Some(r)) if kind_of(l) != kind_of(r) => {
                out.push(entry(path, l, CompareChange::Deleted, false));
                out.push(entry(path, r, CompareChange::Added, false));
            }
            (Some(l), Some(r)) => {
                let props = l.props != r.props;
                if l.content != r.content {
                    out.push(entry(path, r, CompareChange::Modified, props));
                } else if props {
                    out.push(entry(path, r, CompareChange::Normal, true));
                }
            }
            (None, None) => {}
        }
    }
    out
}

impl FakeEngine {
    pub(super) async fn do_compare_summarize(
        &self,
        left: CompareLeft<'_>,
        right_url: &str,
        right_rev: Option<Rev>,
        ctx: &OpContext,
    ) -> Result<CompareSummary, SvnError> {
        ctx.check()?;
        let (right_session, right_path) = self.locate(right_url)?;
        right_session.ensure_authenticated(&ctx.cancel).await?;
        let right_rev = match right_rev {
            Some(r) => Revnum(r.0),
            None => right_session.youngest_rev(&ctx.cancel).await?,
        };
        let (left_tree, left_rev) = match left {
            CompareLeft::Url { url, rev } => {
                let (session, path) = self.locate(url)?;
                same_repository(&session, &right_session, right_url)?;
                let rev = match rev {
                    Some(r) => Revnum(r.0),
                    None => session.youngest_rev(&ctx.cancel).await?,
                };
                (
                    read_tree(&session, &path, rev, &ctx.cancel).await?,
                    Some(Rev(rev.0)),
                )
            }
            CompareLeft::Wc(wc) => (self.working_tree(&wc.root, right_url)?, None),
        };
        let right_tree = read_tree(&right_session, &right_path, right_rev, &ctx.cancel).await?;
        Ok(CompareSummary {
            left_rev,
            right_rev: Rev(right_rev.0),
            entries: summarize(&left_tree, &right_tree),
        })
    }

    /// The working tree of a fake working copy: versioned nodes as they are now (additions in,
    /// deletions and unversioned files out).
    fn working_tree(&self, root: &std::path::Path, right_url: &str) -> Result<Tree, SvnError> {
        let right = RepoUrl::parse(right_url)?.to_string();
        self.with_wc(root, |wc| {
            let repo = wc.repo_root_url.trim_end_matches('/');
            if right != repo && !right.starts_with(&format!("{repo}/")) {
                return Err(other_repository(right_url));
            }
            let mut tree = Tree::new();
            tree.insert(
                RelPath::root(),
                Side {
                    content: None,
                    props: BTreeMap::new(),
                },
            );
            for (path, n) in &wc.nodes {
                if matches!(n.sched, Sched::Deleted | Sched::Unversioned) {
                    continue;
                }
                let props = n
                    .props
                    .iter()
                    .map(|(k, v)| (k.clone(), Bytes::from(v.clone().into_bytes())))
                    .collect();
                let content = match n.kind {
                    FileKind::Dir => None,
                    _ => Some(
                        n.work
                            .clone()
                            .or_else(|| n.base.clone())
                            .unwrap_or_default(),
                    ),
                };
                tree.insert(path.clone(), Side { content, props });
            }
            Ok(tree)
        })
    }
}

fn other_repository(url: &str) -> SvnError {
    SvnError::new(
        ErrorCode::WcInvalidOperation,
        format!("{url} is not in this repository; compare folders of one repository."),
    )
}

fn same_repository(
    left: &FakeRaSession,
    right: &FakeRaSession,
    right_url: &str,
) -> Result<(), SvnError> {
    if left.repos_root() == right.repos_root() {
        Ok(())
    } else {
        Err(other_repository(right_url))
    }
}

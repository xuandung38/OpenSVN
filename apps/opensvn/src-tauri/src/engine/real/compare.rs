//! Compare with branch / another folder (`svn diff --summarize`), through `svn_client::summarize`.

use opensvn_ipc::{CompareChange, CompareEntry, CompareSummary, EntryKind, Rev};
use svn_client::{Summary, SummaryKind, SummaryOptions};
use svn_ra_api::RaSession;
use svn_types::{ErrorCode, NodeKind, RepoUrl, Revnum, SvnError};

use super::{ConnectOptions, RealEngine, wc_read};
use crate::engine::context::OpContext;
use crate::engine::facade::{CompareLeft, EngineResult};

/// `right` lies in the repository of `session`.
fn same_repository(session: &dyn RaSession, right: &RepoUrl) -> EngineResult<()> {
    let root = session.repos_root().to_string();
    let root = root.trim_end_matches('/');
    let other = right.to_string();
    if other == root || other.starts_with(&format!("{root}/")) {
        Ok(())
    } else {
        Err(SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!("{other} is not in the repository {root}; compare folders of one repository."),
        ))
    }
}

/// The summary as the UI receives it.
pub(crate) fn to_ipc(summary: Summary, left_rev: Option<Rev>) -> CompareSummary {
    CompareSummary {
        left_rev,
        right_rev: Rev(summary.right_rev.0),
        entries: summary
            .entries
            .into_iter()
            .map(|e| CompareEntry {
                path: e.path.as_str().to_owned(),
                kind: if e.node_kind == NodeKind::Dir {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                },
                change: match e.kind {
                    SummaryKind::Added => CompareChange::Added,
                    SummaryKind::Deleted => CompareChange::Deleted,
                    SummaryKind::Modified => CompareChange::Modified,
                    SummaryKind::Normal => CompareChange::Normal,
                },
                props_changed: e.props_changed,
            })
            .collect(),
    }
}

impl RealEngine {
    pub(super) async fn do_compare_summarize(
        &self,
        left: CompareLeft<'_>,
        right_url: &str,
        right_rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<CompareSummary> {
        ctx.check()?;
        let right = RepoUrl::parse(right_url)?;
        let right_rev = right_rev.map(|r| Revnum(r.0));
        let opts = SummaryOptions::default();
        match left {
            CompareLeft::Url { url, rev } => {
                let url = RepoUrl::parse(url)?;
                let session = self
                    .connector
                    .connect_with(&url, &ConnectOptions::at_url(ctx), &ctx.cancel)
                    .await?;
                same_repository(session.as_ref(), &right)?;
                let left_rev = match rev {
                    Some(r) => Revnum(r.0),
                    None => session.youngest_rev(&ctx.cancel).await?,
                };
                let summary = svn_client::summarize_repos(
                    session.as_ref(),
                    left_rev,
                    &right,
                    right_rev,
                    opts,
                    &ctx.cancel,
                )
                .await?;
                Ok(to_ipc(summary, Some(Rev(left_rev.0))))
            }
            CompareLeft::Wc(wc) => {
                let info = wc_read::wc_info(&wc.root, ctx).await?;
                let url = RepoUrl::parse(&info.url)?;
                let session = self
                    .connector
                    .connect_with(&url, &ConnectOptions::at_url(ctx), &ctx.cancel)
                    .await?;
                same_repository(session.as_ref(), &right)?;
                let summary = svn_client::summarize_wc(
                    &self.client_ctx(ctx),
                    session.as_ref(),
                    &wc.root,
                    &right,
                    right_rev,
                    opts,
                    &ctx.cancel,
                )
                .await?;
                Ok(to_ipc(summary, None))
            }
        }
    }
}

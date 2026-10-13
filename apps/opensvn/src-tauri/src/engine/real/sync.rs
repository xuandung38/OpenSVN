//! Operations that bring a working copy to a repository state through `svn-client`: checkout,
//! update, switch and depth changes.
//!
//! Every drive runs on a session anchored at the URL it works on
//! ([`ConnectOptions::at_url`]): `svn-client` checks out the session URL and requires an update's
//! session to point at the working copy root. Cancellation is cooperative: the operation
//! context's token goes to `svn-client`, which checks it between editor steps and while waiting
//! for the server. A cancelled or failed checkout leaves the root recorded as incomplete, so a
//! later update finishes it.

use std::path::PathBuf;
use std::sync::Arc;

use opensvn_ipc::{Depth, Rev, UpdateRequest, WcFormat};
use svn_ra_api::RaSession;
use svn_types::{ErrorCode, RelPath, RepoUrl, Revnum, SvnError};

use super::progress::{PHASE_CHECKOUT, PHASE_UPDATE, client_progress};
use super::{ConnectOptions, RealEngine, wc_read};
use crate::engine::context::OpContext;
use crate::engine::facade::{CheckoutPlan, EngineResult, WcInfo, WcRef};

/// Error of a feature the real engine does not offer yet; the message is shown to the user.
fn unsupported(message: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::WcInvalidOperation, message)
}

/// `svn-client` depth of an IPC depth.
fn client_depth(depth: Depth) -> svn_types::Depth {
    match depth {
        Depth::Empty => svn_types::Depth::Empty,
        Depth::Files => svn_types::Depth::Files,
        Depth::Immediates => svn_types::Depth::Immediates,
        Depth::Infinity => svn_types::Depth::Infinity,
    }
}

/// `url` without trailing slashes, for comparisons.
fn trimmed(url: &RepoUrl) -> String {
    url.to_string().trim_end_matches('/').to_owned()
}

/// Whether `url` is the repository root `root` or below it (segment by segment).
fn is_inside(root: &RepoUrl, url: &RepoUrl) -> bool {
    let (root, url) = (trimmed(root), trimmed(url));
    url == root || url.starts_with(&format!("{root}/"))
}

impl RealEngine {
    /// Opens an authenticated session anchored at `url`.
    async fn session_at(&self, url: &str, ctx: &OpContext) -> EngineResult<Arc<dyn RaSession>> {
        ctx.check()?;
        let parsed = RepoUrl::parse(url)?;
        let session = self
            .connector
            .connect_with(&parsed, &ConnectOptions::at_url(ctx), &ctx.cancel)
            .await?;
        // A root-anchored session would silently check out or update the wrong tree. The server
        // may spell the root part differently (case-insensitive servers), hence the case fold.
        if !trimmed(session.session_url()).eq_ignore_ascii_case(&trimmed(&parsed)) {
            return Err(SvnError::new(
                ErrorCode::Internal,
                format!(
                    "the connector opened a session at {} instead of {parsed}",
                    session.session_url()
                ),
            ));
        }
        session.ensure_authenticated(&ctx.cancel).await?;
        Ok(session)
    }

    /// `svn-client` context of an operation that reports progress under `phase`.
    fn sync_ctx(&self, ctx: &OpContext, phase: &'static str) -> svn_client::ClientCtx {
        let mut client = self.client_ctx(ctx);
        client.progress = Some(client_progress(&ctx.progress, phase));
        client
    }

    pub(super) async fn do_checkout(
        &self,
        plan: &CheckoutPlan,
        ctx: &OpContext,
    ) -> EngineResult<WcInfo> {
        // Cancellation wins over everything else, as for every wired operation.
        ctx.check()?;
        if plan.format != WcFormat::V31 {
            return Err(SvnError::new(
                ErrorCode::WcFormatUnsupported,
                "only working copy format 31 can be created",
            ));
        }
        if plan.sparse.as_ref().is_some_and(|s| !s.is_empty()) {
            return Err(unsupported(
                "sparse checkouts (a different depth per folder) are not supported yet; check \
                 out with a single depth instead",
            ));
        }
        if !plan.path.is_absolute() {
            return Err(SvnError::new(
                ErrorCode::Internal,
                "checkout path must be absolute",
            ));
        }
        let session = self.session_at(&plan.url, ctx).await?;
        svn_client::checkout(
            &self.sync_ctx(ctx, PHASE_CHECKOUT),
            session.as_ref(),
            &plan.path,
            plan.rev.map(|r| Revnum(r.0)),
            client_depth(plan.depth),
            false,
            &ctx.cancel,
        )
        .await?;
        wc_read::wc_info(&plan.path, ctx).await
    }

    pub(super) async fn do_update(
        &self,
        wc: &WcRef,
        req: &UpdateRequest,
        ctx: &OpContext,
    ) -> EngineResult<Rev> {
        ctx.check()?;
        let targets: Vec<PathBuf> = match req.paths.as_deref() {
            None | Some([]) => vec![wc.root.clone()],
            Some(paths) => paths
                .iter()
                .map(|p| Ok(wc.root.join(RelPath::new(p)?.as_str())))
                .collect::<EngineResult<_>>()?,
        };
        let info = wc_read::wc_info(&wc.root, ctx).await?;
        let session = self.session_at(&info.url, ctx).await?;
        // Every path goes to the same revision, even when a commit lands in between.
        let rev = match req.rev {
            Some(r) => Revnum(r.0),
            None => session.youngest_rev(&ctx.cancel).await?,
        };
        let depth = req.depth.map_or(svn_types::Depth::Unknown, client_depth);
        let client = self.sync_ctx(ctx, PHASE_UPDATE);
        for target in &targets {
            svn_client::update(
                &client,
                session.as_ref(),
                target,
                Some(rev),
                depth,
                &ctx.cancel,
            )
            .await?;
        }
        Ok(Rev(rev.0))
    }

    pub(super) async fn do_switch(
        &self,
        wc: &WcRef,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<Rev> {
        ctx.check()?;
        let target = RepoUrl::parse(url)?;
        let info = wc_read::wc_info(&wc.root, ctx).await?;
        let session = self.session_at(&info.url, ctx).await?;
        if !is_inside(session.repos_root(), &target) {
            return Err(SvnError::new(
                ErrorCode::WcInvalidOperation,
                format!(
                    "cannot switch to {target}: it is not in this working copy's repository ({}); \
                     check out the other repository instead",
                    session.repos_root()
                ),
            ));
        }
        let rev = match rev {
            Some(r) => Revnum(r.0),
            None => session.youngest_rev(&ctx.cancel).await?,
        };
        svn_client::switch(
            &self.sync_ctx(ctx, PHASE_UPDATE),
            session.as_ref(),
            &wc.root,
            &target,
            Some(rev),
            svn_types::Depth::Unknown,
            &ctx.cancel,
        )
        .await?;
        Ok(Rev(rev.0))
    }

    pub(super) async fn do_set_depth(
        &self,
        _wc: &WcRef,
        _path: &str,
        _depth: Depth,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        ctx.check()?;
        Err(unsupported(
            "changing the depth of a working copy folder is not supported yet; check the folder \
             out again with the depth you need",
        ))
    }
}

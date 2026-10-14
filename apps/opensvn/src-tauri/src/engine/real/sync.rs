//! Operations that bring a working copy to a repository state through `svn-client`: checkout,
//! update, switch and depth changes.
//!
//! Every drive runs on a session anchored at the URL it works on
//! ([`ConnectOptions::at_url`]): `svn-client` checks out the session URL and requires an update's
//! session to point at the working copy root. Cancellation is cooperative: the operation
//! context's token goes to `svn-client`, which checks it between editor steps and while waiting
//! for the server. A cancelled or failed checkout leaves the root recorded as incomplete, so a
//! later update finishes it.
//!
//! Externals are never fetched: checkout, update and switch behave like
//! `svn --ignore-externals` whatever [`CheckoutPlan::ignore_externals`] says, and
//! [`RealEngine`]'s capabilities report `externals: false` so the UI can say so. The
//! `svn:externals` property itself is checked out like any other property. Support is tracked in
//! <https://github.com/xuandung38/OpenSVN/issues/97>.

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

/// Splits a URL path into the repository root part (compared case-insensitively against
/// `root_path`) and the rest, which is `""` or starts with `/`.
fn split_root<'a>(path: &'a str, root_path: &str) -> Option<&'a str> {
    let head = path.get(..root_path.len())?;
    let rest = &path[root_path.len()..];
    (head.eq_ignore_ascii_case(root_path) && (rest.is_empty() || rest.starts_with('/')))
        .then_some(rest)
}

/// Whether the session opened at `got` is the requested `wanted` URL of the repository rooted at
/// `root`. Scheme, host and port compare as the URL parser normalized them (case-insensitively).
/// The repository root part of the path compares case-insensitively, because a server on a
/// case-insensitive file system (VisualSVN) may spell it differently. The path below the root
/// compares exactly: repository paths are case-sensitive.
fn same_location(root: &RepoUrl, wanted: &RepoUrl, got: &RepoUrl) -> bool {
    let (w, g) = (wanted.as_url(), got.as_url());
    if w.scheme() != g.scheme()
        || w.host_str() != g.host_str()
        || w.port_or_known_default() != g.port_or_known_default()
    {
        return false;
    }
    let root_path = root.as_url().path().trim_end_matches('/');
    let rest = |p: &str| split_root(p.trim_end_matches('/'), root_path).map(str::to_owned);
    matches!((rest(w.path()), rest(g.path())), (Some(a), Some(b)) if a == b)
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
        // A root-anchored session would silently check out or update the wrong tree.
        if !same_location(session.repos_root(), &parsed, session.session_url()) {
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

    /// Checks out `plan.url` into `plan.path`. Externals are never fetched, whatever
    /// `plan.ignore_externals` says (see the module documentation).
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
        if !plan.store_pristine {
            return Err(SvnError::new(
                ErrorCode::WcFormatUnsupported,
                "working copies without pristine copies (format 32) cannot be created yet; \
                 check out with pristine copies instead",
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

    /// Updates the given paths, all to the same revision. Externals are never fetched.
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

    /// `svn update --set-depth DEPTH PATH`: crops (never removing locally modified items) or
    /// widens the folder, then updates it to HEAD at the new depth.
    pub(super) async fn do_set_depth(
        &self,
        wc: &WcRef,
        path: &str,
        depth: Depth,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        ctx.check()?;
        let rel = RelPath::new(path)?;
        let target = svn_types::safe_join(&wc.root, &rel)?;
        let info = wc_read::wc_info(&wc.root, ctx).await?;
        let session = self.session_at(&info.url, ctx).await?;
        svn_client::set_depth(
            &self.sync_ctx(ctx, PHASE_UPDATE),
            session.as_ref(),
            &target,
            None,
            client_depth(depth),
            &ctx.cancel,
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn url(s: &str) -> RepoUrl {
        RepoUrl::parse(s).unwrap()
    }

    #[test]
    fn session_location_folds_case_only_for_scheme_host_and_root() {
        let root = url("https://svn.example.com/svn/Repo");
        let same = |wanted: &str, got: &str| same_location(&root, &url(wanted), &url(got));
        // Host, scheme and the repository root part may differ in case.
        assert!(same(
            "HTTPS://SVN.Example.com/SVN/repo/trunk",
            "https://svn.example.com/svn/Repo/trunk"
        ));
        assert!(same(
            "https://svn.example.com/svn/repo",
            "https://svn.example.com/svn/Repo"
        ));
        assert!(same(
            "https://svn.example.com:443/svn/Repo/trunk/",
            "https://svn.example.com/svn/Repo/trunk"
        ));
        // The path below the root is case-sensitive.
        assert!(!same(
            "https://svn.example.com/svn/Repo/Trunk",
            "https://svn.example.com/svn/Repo/trunk"
        ));
        // A root-anchored session is not the requested URL.
        assert!(!same(
            "https://svn.example.com/svn/Repo/trunk",
            "https://svn.example.com/svn/Repo"
        ));
        // Another host, port or scheme, or a path that only shares a prefix with the root.
        assert!(!same(
            "https://other.example.com/svn/Repo/trunk",
            "https://svn.example.com/svn/Repo/trunk"
        ));
        assert!(!same(
            "https://svn.example.com:8443/svn/Repo/trunk",
            "https://svn.example.com/svn/Repo/trunk"
        ));
        assert!(!same(
            "http://svn.example.com/svn/Repo/trunk",
            "https://svn.example.com/svn/Repo/trunk"
        ));
        assert!(!same(
            "https://svn.example.com/svn/Repox/trunk",
            "https://svn.example.com/svn/Repo/trunk"
        ));
    }

    #[test]
    fn a_repository_at_the_server_root_compares_the_whole_path_exactly() {
        let root = url("https://svn.example.com");
        let same = |wanted: &str, got: &str| same_location(&root, &url(wanted), &url(got));
        assert!(same(
            "https://SVN.example.com/trunk",
            "https://svn.example.com/trunk"
        ));
        assert!(!same(
            "https://svn.example.com/Trunk",
            "https://svn.example.com/trunk"
        ));
    }
}

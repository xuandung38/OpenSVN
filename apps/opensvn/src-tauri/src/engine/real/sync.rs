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
//! Externals (`svn:externals`) are fetched, updated and removed after every checkout, update and
//! switch, like svn does; a checkout with [`CheckoutPlan::ignore_externals`] skips them
//! (`svn --ignore-externals`). Every external session, also one on another host, is opened
//! through the same [`SessionConnector`](super::SessionConnector) and [`ConnectOptions`] as the
//! main session, so the TLS default-deny and sign-in rules apply unchanged
//! ([`ConnectorOpener`]). A failed external, or one removed from its definition but kept for its
//! local changes, does not fail the operation (svn warns too): it is logged and reported as a
//! progress sample with phase [`PHASE_EXTERNAL_WARNING`] and the external's path.
//!
//! A checkout or update whose server cannot be reached any more (Wi-Fi gone, VPN reconnecting,
//! the system asleep, the server restarting) waits for it: stage
//! [`OpStage::WaitingForConnection`] with the timing, for up to the connector's
//! [`SessionConnector::reconnect_wait`], then resumes at the same revision (see
//! [`svn_client::netwait`]). "Retry now" and the system waking up wake it through
//! [`OpContext::retry_now`]. Writes never wait.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use opensvn_ipc::{
    ConnectionWait, Depth, OpStage, Rev, SparseDepth, SparseEntry, UpdateRequest, WcFormat,
};
use svn_ra_api::RaSession;
use svn_types::{CancelToken, ErrorCode, RelPath, RepoUrl, Revnum, SvnError};

use super::progress::{PHASE_CHECKOUT, PHASE_UPDATE, client_progress};
use super::{ConnectOptions, RealEngine, SessionConnector, wc_read};
use crate::engine::context::{OpContext, ProgressUpdate};
use crate::engine::facade::{CheckoutPlan, EngineResult, WcInfo, WcRef};

/// `svn-client` depth of an IPC depth.
fn client_depth(depth: Depth) -> svn_types::Depth {
    match depth {
        Depth::Empty => svn_types::Depth::Empty,
        Depth::Files => svn_types::Depth::Files,
        Depth::Immediates => svn_types::Depth::Immediates,
        Depth::Infinity => svn_types::Depth::Infinity,
    }
}

/// The folders of a sparse checkout in the order they are applied (parents first, see
/// [`crate::engine::mapping::sparse_steps`]), with `svn-client` depths.
fn sparse_steps(
    entries: &[SparseEntry],
    with_root: bool,
) -> EngineResult<Vec<(RelPath, svn_types::Depth)>> {
    Ok(crate::engine::mapping::sparse_steps(entries, with_root)?
        .into_iter()
        .map(|(rel, depth)| {
            let depth = match depth {
                SparseDepth::Empty => svn_types::Depth::Empty,
                SparseDepth::Files => svn_types::Depth::Files,
                SparseDepth::Immediates => svn_types::Depth::Immediates,
                SparseDepth::Infinity => svn_types::Depth::Infinity,
                SparseDepth::Exclude => svn_types::Depth::Exclude,
            };
            (rel, depth)
        })
        .collect())
}

/// IPC folder depth of a recorded directory depth.
fn sparse_depth(depth: svn_types::Depth) -> SparseDepth {
    match depth {
        svn_types::Depth::Empty => SparseDepth::Empty,
        svn_types::Depth::Files => SparseDepth::Files,
        svn_types::Depth::Immediates => SparseDepth::Immediates,
        _ => SparseDepth::Infinity,
    }
}

/// `url` without trailing slashes, for comparisons.
fn trimmed(url: &RepoUrl) -> String {
    url.to_string().trim_end_matches('/').to_owned()
}

/// Whether the session opened at `got` is the requested `wanted` URL of the repository rooted at
/// `root`. Scheme, host and port compare as the URL parser normalized them (case-insensitively).
/// The paths compare with the rule the protocol layer applies to the server's root
/// ([`svn_protocol::segments_below_root`]): segments are percent-decoded, the repository root
/// part may differ in ASCII case (a server on a case-insensitive file system, VisualSVN, may
/// spell it differently), and the decoded segments below the root compare exactly (repository
/// paths are case-sensitive).
fn same_location(root: &RepoUrl, wanted: &RepoUrl, got: &RepoUrl) -> bool {
    let (w, g) = (wanted.as_url(), got.as_url());
    if w.scheme() != g.scheme()
        || w.host_str() != g.host_str()
        || w.port_or_known_default() != g.port_or_known_default()
    {
        return false;
    }
    let root_path = root.as_url().path();
    let below = |p: &str| {
        svn_protocol::segments_below_root(root_path, p)
            .ok()
            .flatten()
    };
    matches!((below(w.path()), below(g.path())), (Some(a), Some(b)) if a == b)
}

/// Whether `url` is the repository root `root` or below it (segment by segment).
fn is_inside(root: &RepoUrl, url: &RepoUrl) -> bool {
    let (root, url) = (trimmed(root), trimmed(url));
    url == root || url.starts_with(&format!("{root}/"))
}

/// Phase of the progress sample sent for an external that failed or kept local changes; its
/// `current_path` is the external's path in the working copy. A notice: never coalesced away.
pub(super) use crate::engine::context::PHASE_EXTERNAL_WARNING;

/// Opens an authenticated session anchored exactly at `url` through `connector`.
async fn open_at(
    connector: &dyn SessionConnector,
    url: &RepoUrl,
    options: &ConnectOptions,
    cancel: &CancelToken,
) -> Result<Arc<dyn RaSession>, SvnError> {
    cancel.check()?;
    let session = connector.connect_with(url, options, cancel).await?;
    // A root-anchored session would silently check out or update the wrong tree.
    if !same_location(session.repos_root(), url, session.session_url()) {
        return Err(SvnError::new(
            ErrorCode::Internal,
            format!(
                "the connector opened a session at {} instead of {url}",
                session.session_url()
            ),
        ));
    }
    session.ensure_authenticated(cancel).await?;
    Ok(session)
}

/// Sessions for externals, opened exactly like the operation's own session: same connector
/// (transport, trust store, credentials), same interactivity.
struct ConnectorOpener {
    connector: Arc<dyn SessionConnector>,
    options: ConnectOptions,
}

#[async_trait]
impl svn_client::externals::SessionOpener for ConnectorOpener {
    async fn open(
        &self,
        url: &RepoUrl,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, SvnError> {
        open_at(self.connector.as_ref(), url, &self.options, cancel).await
    }
}

/// Logs the externals that need attention and reports each one as a progress sample.
fn report_external_warnings(summary: &svn_client::UpdateSummary, ctx: &OpContext) {
    for w in summary.external_warnings() {
        tracing::warn!(
            path = %w.path,
            action = ?w.action,
            "external: {}",
            w.message.as_deref().unwrap_or("no details")
        );
        ctx.progress.report(ProgressUpdate {
            phase: PHASE_EXTERNAL_WARNING.to_owned(),
            current_path: Some(w.path.as_str().to_owned()),
            ..ProgressUpdate::default()
        });
    }
}

impl RealEngine {
    /// Opens an authenticated session anchored at `url`.
    async fn session_at(&self, url: &str, ctx: &OpContext) -> EngineResult<Arc<dyn RaSession>> {
        ctx.check()?;
        let parsed = RepoUrl::parse(url)?;
        open_at(
            self.connector.as_ref(),
            &parsed,
            &ConnectOptions::at_url(ctx),
            &ctx.cancel,
        )
        .await
    }

    /// `svn-client` context of an operation that reports progress under `phase`; externals are
    /// fetched unless `ignore_externals`.
    fn sync_ctx(
        &self,
        ctx: &OpContext,
        phase: &'static str,
        ignore_externals: bool,
    ) -> svn_client::ClientCtx {
        let mut client = self.client_ctx(ctx);
        client.progress = Some(client_progress(&ctx.progress, phase));
        // The app follows the progress sink only: no list of every notification is kept.
        client.keep_events = false;
        client.network_wait = self.network_wait(ctx, phase);
        if !ignore_externals {
            client.externals = Some(Arc::new(ConnectorOpener {
                connector: Arc::clone(&self.connector),
                options: ConnectOptions::at_url(ctx),
            }));
        }
        client
    }

    /// How a drive of this operation waits for a lost connection: for the connector's
    /// [`SessionConnector::reconnect_wait`], reporting [`OpStage::WaitingForConnection`] and
    /// woken by [`OpContext::retry_now`]. `None` when the connector does not wait.
    fn network_wait(
        &self,
        ctx: &OpContext,
        phase: &'static str,
    ) -> Option<svn_client::NetworkWait> {
        let max_wait = self.connector.reconnect_wait()?;
        let progress = ctx.progress.clone();
        let secs = |d: std::time::Duration| {
            u32::try_from(d.as_millis().div_ceil(1000)).unwrap_or(u32::MAX)
        };
        let on_state = move |state: svn_client::WaitState| match state {
            svn_client::WaitState::Waiting {
                retry_in,
                waited,
                max_wait,
                attempt,
            } => progress.report(ProgressUpdate {
                phase: phase.to_owned(),
                stage: Some(OpStage::WaitingForConnection),
                connection_wait: Some(ConnectionWait {
                    retry_in_secs: secs(retry_in),
                    waited_secs: secs(waited),
                    max_wait_secs: secs(max_wait),
                    attempt,
                }),
                ..ProgressUpdate::default()
            }),
            svn_client::WaitState::Restored => {
                progress.report(ProgressUpdate::stage(phase, OpStage::Fetching));
            }
            svn_client::WaitState::Probing { .. } | svn_client::WaitState::GaveUp => {}
        };
        Some(svn_client::NetworkWait {
            max_wait,
            retry_now: Arc::clone(&ctx.retry_now),
            on_state: Some(Arc::new(on_state)),
            ..svn_client::NetworkWait::default()
        })
    }

    /// Checks out `plan.url` into `plan.path`, with its externals unless
    /// `plan.ignore_externals`.
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
        let sparse = sparse_steps(plan.sparse.as_deref().unwrap_or_default(), false)?;
        if !plan.path.is_absolute() {
            return Err(SvnError::new(
                ErrorCode::Internal,
                "checkout path must be absolute",
            ));
        }
        ctx.progress
            .report(ProgressUpdate::stage(PHASE_CHECKOUT, OpStage::Connecting));
        let session = self.session_at(&plan.url, ctx).await?;
        ctx.progress
            .report(ProgressUpdate::stage(PHASE_CHECKOUT, OpStage::Fetching));
        if sparse.is_empty() {
            let summary = svn_client::checkout(
                &self.sync_ctx(ctx, PHASE_CHECKOUT, plan.ignore_externals),
                session.as_ref(),
                &plan.path,
                plan.rev.map(|r| Revnum(r.0)),
                client_depth(plan.depth),
                false,
                &ctx.cancel,
            )
            .await?;
            report_external_warnings(&summary, ctx);
        } else {
            self.sparse_checkout(plan, &sparse, session.as_ref(), ctx)
                .await?;
        }
        ctx.progress
            .report(ProgressUpdate::stage(PHASE_CHECKOUT, OpStage::Finishing));
        wc_read::wc_info(&plan.path, ctx).await
    }

    /// A checkout with a depth per folder: the root at `plan.depth`, then every folder of
    /// `steps` (parents first) like `svn update --parents --set-depth`, all at one revision.
    /// The working copy ends up as svn 1.15 leaves it after the same commands. Externals come
    /// last, through an update of the root at that revision, so only those of folders the
    /// working copy has are fetched.
    async fn sparse_checkout(
        &self,
        plan: &CheckoutPlan,
        steps: &[(RelPath, svn_types::Depth)],
        session: &dyn RaSession,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        let rev = match plan.rev {
            Some(r) => Revnum(r.0),
            None => session.youngest_rev(&ctx.cancel).await?,
        };
        let client = self.sync_ctx(ctx, PHASE_CHECKOUT, true);
        svn_client::checkout(
            &client,
            session,
            &plan.path,
            Some(rev),
            client_depth(plan.depth),
            false,
            &ctx.cancel,
        )
        .await?;
        for (rel, depth) in steps {
            ctx.check()?;
            let target = svn_types::safe_join(&plan.path, rel)?;
            if *depth == svn_types::Depth::Exclude {
                // Only what the working copy has can be excluded; the rest stays out anyway.
                svn_client::set_depth(&client, session, &target, Some(rev), *depth, &ctx.cancel)
                    .await?;
            } else {
                svn_client::set_depth_parents(
                    &client,
                    session,
                    &target,
                    Some(rev),
                    *depth,
                    &ctx.cancel,
                )
                .await?;
            }
        }
        if !plan.ignore_externals {
            let summary = svn_client::update(
                &self.sync_ctx(ctx, PHASE_CHECKOUT, false),
                session,
                &plan.path,
                Some(rev),
                svn_types::Depth::Unknown,
                &ctx.cancel,
            )
            .await?;
            report_external_warnings(&summary, ctx);
        }
        Ok(())
    }

    /// Updates the given paths, all to the same revision, with their externals unless
    /// `ignore_externals`.
    pub(super) async fn do_update(
        &self,
        wc: &WcRef,
        req: &UpdateRequest,
        ignore_externals: bool,
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
        ctx.progress
            .report(ProgressUpdate::stage(PHASE_UPDATE, OpStage::Connecting));
        let session = self.session_at(&info.url, ctx).await?;
        ctx.progress
            .report(ProgressUpdate::stage(PHASE_UPDATE, OpStage::Fetching));
        // Every path goes to the same revision, even when a commit lands in between.
        let rev = match req.rev {
            Some(r) => Revnum(r.0),
            None => session.youngest_rev(&ctx.cancel).await?,
        };
        let depth = req.depth.map_or(svn_types::Depth::Unknown, client_depth);
        let client = self.sync_ctx(ctx, PHASE_UPDATE, ignore_externals);
        for target in &targets {
            let summary = svn_client::update(
                &client,
                session.as_ref(),
                target,
                Some(rev),
                depth,
                &ctx.cancel,
            )
            .await?;
            report_external_warnings(&summary, ctx);
        }
        ctx.progress
            .report(ProgressUpdate::stage(PHASE_UPDATE, OpStage::Finishing));
        Ok(Rev(rev.0))
    }

    pub(super) async fn do_switch(
        &self,
        wc: &WcRef,
        url: &str,
        rev: Option<Rev>,
        ignore_ancestry: bool,
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
        let summary = svn_client::switch(
            &self.sync_ctx(ctx, PHASE_UPDATE, false),
            session.as_ref(),
            &wc.root,
            &target,
            Some(rev),
            svn_types::Depth::Unknown,
            ignore_ancestry,
            &ctx.cancel,
        )
        .await?;
        report_external_warnings(&summary, ctx);
        Ok(Rev(rev.0))
    }

    /// `svn update --set-depth DEPTH PATH`: crops (never removing locally modified items) or
    /// widens the folder, then updates it to HEAD at the new depth. A folder the working copy
    /// does not have yet (outside its parent's depth, or excluded) is brought in with the
    /// folders above it that are missing too (`svn update --parents --set-depth`).
    pub(super) async fn do_set_depth(
        &self,
        wc: &WcRef,
        path: &str,
        depth: Depth,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_set_depth_inner(wc, path, client_depth(depth), ctx)
            .await
    }

    /// The recorded depth of every folder below the root (see [`crate::engine::EngineFacade::wc_depths`]).
    pub(super) async fn do_wc_depths(
        &self,
        wc: &WcRef,
        ctx: &OpContext,
    ) -> EngineResult<Vec<SparseEntry>> {
        ctx.check()?;
        let root = wc.root.clone();
        wc_read::blocking(move || {
            let db = wc_read::open_db(&root)?;
            let root_depth = db
                .read_base(&RelPath::root())?
                .and_then(|n| n.depth)
                .unwrap_or(svn_types::Depth::Infinity);
            let mut out = vec![SparseEntry {
                path: String::new(),
                depth: sparse_depth(root_depth),
            }];
            let mut stack = vec![RelPath::root()];
            while let Some(dir) = stack.pop() {
                let mut subdirs = Vec::new();
                for node in db.base_children(&dir)? {
                    if node.kind != svn_types::NodeKind::Dir || node.file_external {
                        continue;
                    }
                    let depth = match node.presence {
                        svn_wc::db::Presence::Excluded => SparseDepth::Exclude,
                        p if p.is_present() => {
                            sparse_depth(node.depth.unwrap_or(svn_types::Depth::Infinity))
                        }
                        _ => continue,
                    };
                    if depth != SparseDepth::Exclude {
                        subdirs.push(node.relpath.clone());
                    }
                    out.push(SparseEntry {
                        path: node.relpath.as_str().to_owned(),
                        depth,
                    });
                }
                stack.extend(subdirs);
            }
            out.sort_by_cached_key(|e| e.path.split('/').map(str::to_owned).collect::<Vec<_>>());
            Ok(out)
        })
        .await
    }

    /// Sets the depth of several folders, parents first (see [`crate::engine::EngineFacade::sparse_apply`]).
    pub(super) async fn do_sparse_apply(
        &self,
        wc: &WcRef,
        entries: &[SparseEntry],
        ctx: &OpContext,
    ) -> EngineResult<()> {
        for (rel, depth) in sparse_steps(entries, true)? {
            self.do_set_depth_inner(wc, rel.as_str(), depth, ctx)
                .await?;
        }
        Ok(())
    }

    /// [`Self::do_set_depth`] with every svn depth, `exclude` included.
    pub(super) async fn do_set_depth_inner(
        &self,
        wc: &WcRef,
        path: &str,
        depth: svn_types::Depth,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        ctx.check()?;
        let rel = RelPath::new(path)?;
        let target = svn_types::safe_join(&wc.root, &rel)?;
        // A present folder: the session is anchored at the folder itself, so the drive targets
        // the session root (the report then starts with the folder, as the HTTP layer
        // requires). A missing one is added by a drive anchored at the working copy root.
        let (root, rel2) = (wc.root.clone(), rel.clone());
        let (url, absent) = wc_read::blocking(move || {
            let db = wc_read::open_db(&root)?;
            let (repos_root, _) = svn_client::wc::repos_identity(&db)?;
            let root_url = RepoUrl::parse(&repos_root)?;
            let base = db.read_base(&rel2)?;
            match base.as_ref().filter(|b| b.presence.is_present()) {
                Some(b) => match b.repos_relpath.as_deref() {
                    Some(p) if !p.is_empty() && b.kind == svn_types::NodeKind::Dir => {
                        Ok((root_url.join(&RelPath::new(p)?)?.to_string(), false))
                    }
                    // A file has no depth: svn-client refuses it before talking to the server.
                    Some("") if b.kind == svn_types::NodeKind::Dir => {
                        Ok((root_url.to_string(), false))
                    }
                    _ => Ok((wc_read::describe(&db)?.url, false)),
                },
                None if rel2.is_root() || db.read_node(&rel2)?.is_some_and(|c| c.op_depth > 0) => {
                    Err(SvnError::new(
                        ErrorCode::PathNotFound,
                        format!("'{rel2}' is not under version control"),
                    ))
                }
                None => Ok((wc_read::describe(&db)?.url, true)),
            }
        })
        .await?;
        let session = self.session_at(&url, ctx).await?;
        // `svn-client::set_depth` does not process externals (svn's `--set-depth` would).
        let client = self.sync_ctx(ctx, PHASE_UPDATE, true);
        if absent && depth != svn_types::Depth::Exclude {
            svn_client::set_depth_parents(
                &client,
                session.as_ref(),
                &target,
                None,
                depth,
                &ctx.cancel,
            )
            .await?;
        } else {
            svn_client::set_depth(&client, session.as_ref(), &target, None, depth, &ctx.cancel)
                .await?;
        }
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
    fn a_non_ascii_root_compares_decoded_like_the_protocol_layer() {
        let root = url("https://svn.example.com/svn/D%E1%BB%B1-%C3%81n");
        let same = |wanted: &str, got: &str| same_location(&root, &url(wanted), &url(got));
        // Lowercase or unnecessary escapes, and ASCII case of the root, are the same location.
        assert!(same(
            "https://svn.example.com/svn/d%e1%bb%b1-%c3%81n/trunk",
            "https://svn.example.com/svn/D%E1%BB%B1-%C3%81n/trunk"
        ));
        assert!(same(
            "https://svn.example.com/svn/D%E1%BB%B1-%C3%81n/%74runk/a%20b",
            "https://svn.example.com/svn/D%E1%BB%B1-%C3%81n/trunk/a%20b"
        ));
        assert!(same(
            "https://svn.example.com/svn/Dự-Án/trunk",
            "https://svn.example.com/svn/D%E1%BB%B1-%C3%81n/trunk"
        ));
        // Non-ASCII case is never folded (the protocol layer's fallback is ASCII-only), and the
        // path below the root stays case-sensitive after decoding.
        assert!(!same(
            "https://svn.example.com/svn/D%E1%BB%B1-%C3%A1n/trunk",
            "https://svn.example.com/svn/D%E1%BB%B1-%C3%81n/trunk"
        ));
        assert!(!same(
            "https://svn.example.com/svn/D%E1%BB%B1-%C3%81n/%C3%81",
            "https://svn.example.com/svn/D%E1%BB%B1-%C3%81n/%C3%A1"
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

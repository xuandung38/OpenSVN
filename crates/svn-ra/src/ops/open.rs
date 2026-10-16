//! Remote operation: open. [`HttpRaSession`] is the [`RaSession`] over HTTPv2.
//!
//! Opening sends one `OPTIONS` to the session URL. Everything the session needs later (stub
//! URLs, repository root, UUID, capabilities) comes from that answer and is validated once by
//! [`RaWire`]: every href a later response carries must be on the same origin and below a
//! stub or the repository root, otherwise the operation fails with `UnsafePath` before any
//! request is sent to it.

use std::sync::Arc;

use async_trait::async_trait;
use svn_protocol::options_request;
use svn_ra_api::{
    CommitEditor, CommitObserver, DirListing, FileMeta, FileRevHandler, LocationSegment,
    LogRequest, RaSession, ReportFn, TxnAbort, UpdateRequest,
};
use svn_types::{
    CancelToken, DeltaEditor, Depth, DirEntry, ErrorCode, LockInfo, LogEntry, NodeKind, PropChange,
    RelPath, RepoUrl, Result, Revnum, SvnError,
};
use tokio::io::AsyncWrite;

use super::update::RaWire;
use super::{commit, file_revs, history, locks, log, read, update};
use crate::transport::HttpTransport;

/// Repository session over Subversion's HTTP protocol v2 (mod_dav_svn 1.8 and newer).
#[derive(Debug, Clone)]
pub struct HttpRaSession {
    wire: RaWire,
    repos_root: RepoUrl,
    uuid: String,
}

impl HttpRaSession {
    /// Opens a session at `session_url`: one `OPTIONS` request, which is also where
    /// authentication and certificate decisions happen. Servers without HTTPv2 are refused with
    /// [`ErrorCode::ServerUnsupported`].
    pub async fn open(
        transport: Arc<HttpTransport>,
        session_url: RepoUrl,
        cancel: &CancelToken,
    ) -> Result<Self> {
        Self::from_wire(RaWire::open(transport, session_url, cancel).await?)
    }

    /// Like [`Self::open`], but nothing ever prompts, not even the opening `OPTIONS`: see
    /// [`Self::unattended`]. For background work (remote status, recovery) that must not show
    /// a dialog.
    pub async fn open_unattended(
        transport: Arc<HttpTransport>,
        session_url: RepoUrl,
        cancel: &CancelToken,
    ) -> Result<Self> {
        Self::from_wire(RaWire::open_unattended(transport, session_url, cancel).await?)
    }

    /// The same session rooted at `url`, without a network request: transport, credentials,
    /// capabilities, repository root and UUID are shared. `url` must be on the session's
    /// origin and at or below the repository root, otherwise [`ErrorCode::UnsafePath`].
    pub fn reparent(&self, url: &RepoUrl) -> Result<Self> {
        Ok(Self {
            wire: self.wire.reparent(url)?,
            repos_root: self.repos_root.clone(),
            uuid: self.uuid.clone(),
        })
    }

    /// The same session in unattended mode: every request goes through
    /// [`HttpTransport::send_unattended`], so it never asks the `AuthPrompter` for credentials
    /// or a certificate decision. Only credentials cached by the transport or returned by
    /// `AuthPrompter::stored_credentials`, and certificates already accepted, are used; a
    /// request that would need a prompt fails with `AuthRequired` (or its TLS error).
    #[must_use]
    pub fn unattended(&self) -> Self {
        Self {
            wire: self.wire.clone().unattended(),
            repos_root: self.repos_root.clone(),
            uuid: self.uuid.clone(),
        }
    }

    /// Builds a session from wiring that was already opened (for example with custom
    /// connection limits or update mode).
    pub fn from_wire(wire: RaWire) -> Result<Self> {
        let repos_root = RepoUrl::parse(wire.urls().repository_root()?.as_str())?;
        let uuid = wire
            .caps()
            .uuid
            .clone()
            .filter(|u| valid_uuid(u))
            .ok_or_else(|| {
                SvnError::new(
                    ErrorCode::ProtocolViolation,
                    "server did not send a usable SVN-Repository-UUID",
                )
            })?;
        Ok(Self {
            wire,
            repos_root,
            uuid,
        })
    }

    /// The wiring (transport, capabilities, URL builder) of this session.
    pub fn wire(&self) -> &RaWire {
        &self.wire
    }
}

/// A repository UUID as svnadmin creates it; anything else is not trusted as an identity.
fn valid_uuid(u: &str) -> bool {
    !u.is_empty() && u.len() <= 64 && u.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

#[async_trait]
impl RaSession for HttpRaSession {
    fn repos_root(&self) -> &RepoUrl {
        &self.repos_root
    }

    fn uuid(&self) -> &str {
        &self.uuid
    }

    fn reparented(&self, url: &RepoUrl) -> Result<Box<dyn RaSession>> {
        Ok(Box::new(self.reparent(url)?))
    }
    fn session_url(&self) -> &RepoUrl {
        self.wire.session_url()
    }

    fn authenticated_user(&self) -> Option<String> {
        self.wire
            .transport()
            .authenticated_user(self.wire.session_url().as_url())
    }

    async fn ensure_authenticated(&self, cancel: &CancelToken) -> Result<()> {
        // An OPTIONS goes through the same authentication as every other request, so any
        // prompt happens now, before the caller takes the working-copy lock.
        let req = options_request(self.wire.session_url())?;
        self.wire.send_ok(req, cancel).await?;
        Ok(())
    }

    async fn probe(&self, cancel: &CancelToken) -> Result<()> {
        // Any HTTP answer, even a refusal, means the server is reachable again; a gateway that
        // cannot reach it (502-504) does not. Never prompts: a sign-in the session already has
        // is used, anything else comes back as an error that is not a connection failure.
        let req = options_request(&self.repos_root)?;
        let resp = self.wire.send_unattended(req, cancel).await?;
        let status = resp.status().as_u16();
        if (502..=504).contains(&status) {
            return Err(SvnError::new(
                ErrorCode::Network,
                format!("the server is not reachable through its gateway (HTTP {status})"),
            )
            .with_unreachable());
        }
        Ok(())
    }

    fn reset_connections(&self) {
        self.wire.transport().reset_connections();
    }

    async fn youngest_rev(&self, cancel: &CancelToken) -> Result<Revnum> {
        read::youngest_rev(&self.wire, cancel).await
    }

    async fn check_path(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<NodeKind> {
        Ok(read::stat(&self.wire, path, rev, cancel)
            .await?
            .map_or(NodeKind::None, |e| e.kind))
    }

    async fn stat(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<Option<DirEntry>> {
        read::stat(&self.wire, path, rev, cancel).await
    }

    async fn list(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        depth: Depth,
        cancel: &CancelToken,
    ) -> Result<Vec<DirEntry>> {
        Ok(read::list(&self.wire, path, rev, depth, cancel)
            .await?
            .entries)
    }

    async fn list_counted(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        depth: Depth,
        cancel: &CancelToken,
    ) -> Result<DirListing> {
        read::list(&self.wire, path, rev, depth, cancel).await
    }

    async fn get_file(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        sink: &mut (dyn AsyncWrite + Send + Unpin),
        cancel: &CancelToken,
    ) -> Result<FileMeta> {
        read::get_file(&self.wire, path, rev, sink, cancel).await
    }

    async fn log(
        &self,
        req: &LogRequest,
        on_entry: &mut (dyn FnMut(LogEntry) -> Result<()> + Send),
        cancel: &CancelToken,
    ) -> Result<()> {
        log::log(&self.wire, req, on_entry, cancel).await
    }

    async fn get_locks(
        &self,
        path: &RelPath,
        depth: Depth,
        cancel: &CancelToken,
    ) -> Result<Vec<LockInfo>> {
        locks::get_locks(&self.wire, path, depth, cancel).await
    }

    async fn lock(
        &self,
        path: &RelPath,
        comment: Option<&str>,
        steal: bool,
        base_rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<LockInfo> {
        locks::lock(&self.wire, path, comment, steal, base_rev, cancel).await
    }

    async fn unlock(
        &self,
        path: &RelPath,
        token: Option<&str>,
        break_lock: bool,
        cancel: &CancelToken,
    ) -> Result<()> {
        locks::unlock(&self.wire, path, token, break_lock, cancel).await
    }

    async fn file_revs(
        &self,
        path: &RelPath,
        start: Option<Revnum>,
        end: Option<Revnum>,
        handler: &mut dyn FileRevHandler,
        cancel: &CancelToken,
    ) -> Result<()> {
        file_revs::file_revs(&self.wire, path, start, end, handler, cancel).await
    }

    async fn get_location_segments(
        &self,
        path: &RelPath,
        peg: Option<Revnum>,
        start: Option<Revnum>,
        end: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<Vec<LocationSegment>> {
        history::location_segments(&self.wire, path, peg, start, end, cancel).await
    }

    async fn do_update(
        &self,
        req: UpdateRequest,
        report: ReportFn,
        editor: Box<dyn DeltaEditor>,
        cancel: &CancelToken,
    ) -> Result<()> {
        update::run_update(&self.wire, req, report, editor, cancel).await
    }

    async fn commit_editor(
        &self,
        message: &str,
        rev_props: &[PropChange],
        lock_tokens: &[(String, String)],
        keep_locks: bool,
        observer: Arc<dyn CommitObserver>,
        cancel: &CancelToken,
    ) -> Result<Box<dyn CommitEditor>> {
        commit::start_commit(
            &self.wire,
            message,
            rev_props,
            lock_tokens,
            keep_locks,
            observer,
            cancel,
        )
        .await
    }

    async fn abort_txn(&self, txn_name: &str) -> Result<()> {
        commit::abort_txn(&self.wire, txn_name).await
    }

    async fn abort_txn_checked(&self, txn_name: &str, cancel: &CancelToken) -> Result<TxnAbort> {
        commit::abort_txn_checked(&self.wire, txn_name, cancel).await
    }
}

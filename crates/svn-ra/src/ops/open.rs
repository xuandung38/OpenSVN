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
    CommitEditor, CommitObserver, FileMeta, FileRevHandler, LogRequest, RaSession, ReportFn,
    UpdateRequest,
};
use svn_types::{
    CancelToken, DeltaEditor, Depth, DirEntry, ErrorCode, LockInfo, LogEntry, NodeKind, PropChange,
    RelPath, RepoUrl, Result, Revnum, SvnError,
};
use tokio::io::AsyncWrite;

use super::update::RaWire;
use super::{commit, file_revs, locks, log, read, update};
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
                    ErrorCode::Corrupt,
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

    fn session_url(&self) -> &RepoUrl {
        self.wire.session_url()
    }

    async fn ensure_authenticated(&self, cancel: &CancelToken) -> Result<()> {
        // An OPTIONS goes through the same authentication as every other request, so any
        // prompt happens now, before the caller takes the working-copy lock.
        let req = options_request(self.wire.session_url())?;
        self.wire.send_ok(req, cancel).await?;
        Ok(())
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
}

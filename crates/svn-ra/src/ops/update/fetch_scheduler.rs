//! Parallel `GET` of file contents for skeleton (non send-all) updates.
//!
//! The report connection stays busy for the whole drive, so at most `connections - 1` downloads
//! run at once. Each download streams straight into the editor's window sink: the response body
//! is cut into full-text windows of at most [`WINDOW_BYTES`], and the bounded editor channel
//! pushes back on the network when the editor is slower.

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use svn_protocol::{HttpRequest, Method};
use svn_ra_api::WindowSender;
use svn_types::{CancelToken, ErrorCode, Result, SvnError, TxDeltaWindow};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use url::Url;

use super::wire::RaWire;

/// Size of one full-text window sent to the editor.
const WINDOW_BYTES: usize = 64 * 1024;

/// A running task that is aborted when dropped, so an abandoned drive stops downloading.
#[derive(Debug)]
pub(super) struct DownloadTask(JoinHandle<Result<()>>);

impl DownloadTask {
    /// Waits for the download; a panicked or aborted task is an internal error.
    pub(super) async fn finish(mut self) -> Result<()> {
        match (&mut self.0).await {
            Ok(r) => r,
            Err(e) => Err(SvnError::new(
                ErrorCode::Internal,
                format!("download task failed: {e}"),
            )),
        }
    }
}

impl Drop for DownloadTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts bounded parallel downloads.
#[derive(Debug, Clone)]
pub(super) struct FetchScheduler {
    wire: RaWire,
    permits: Arc<Semaphore>,
    parallelism: usize,
    cancel: CancelToken,
}

impl FetchScheduler {
    /// `connections` is the size of the transport pool; one connection belongs to the report.
    pub(super) fn new(wire: &RaWire, cancel: &CancelToken) -> Self {
        let parallelism = wire.connections().saturating_sub(1).max(1);
        Self {
            wire: wire.clone(),
            permits: Arc::new(Semaphore::new(parallelism)),
            parallelism,
            cancel: cancel.clone(),
        }
    }

    /// Most downloads that are queued or running at once; the driver waits for the oldest one
    /// before starting another, which bounds the number of files the editor keeps open.
    ///
    /// Deep enough that the network keeps fetching while the editor installs a batch of files
    /// (and the editor keeps installing while small files arrive): a shallow window ties both
    /// to the latency of the oldest download, which showed up as idle time on both sides.
    pub(super) fn max_in_flight(&self) -> usize {
        (self.parallelism * 16).clamp(2, 64)
    }

    /// Downloads `url` into `sender`; the sender receives the end marker on success.
    pub(super) fn spawn(&self, url: Url, sender: WindowSender) -> DownloadTask {
        let wire = self.wire.clone();
        let permits = Arc::clone(&self.permits);
        let cancel = self.cancel.clone();
        DownloadTask(tokio::spawn(async move {
            let _permit = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(cancel_error(&cancel)),
                p = permits.acquire_owned() => p.map_err(|_| {
                    SvnError::new(ErrorCode::Internal, "download scheduler closed")
                })?,
            };
            download(&wire, url, &sender, &cancel).await
        }))
    }
}

fn cancel_error(cancel: &CancelToken) -> SvnError {
    cancel
        .check()
        .err()
        .unwrap_or_else(|| SvnError::new(ErrorCode::Cancelled, "operation cancelled"))
}

async fn download(
    wire: &RaWire,
    url: Url,
    sender: &WindowSender,
    cancel: &CancelToken,
) -> Result<()> {
    // GET carries no DAV capability headers (verified against svn 1.15).
    let req = HttpRequest::new(Method::Get, url);
    let mut resp = wire.send_ok(req, cancel).await?;
    let mut buf = BytesMut::new();
    while let Some(chunk) = resp.chunk().await? {
        buf.extend_from_slice(&chunk);
        while buf.len() >= WINDOW_BYTES {
            let piece: Bytes = buf.split_to(WINDOW_BYTES).freeze();
            sender.send(Some(TxDeltaWindow::full_text(piece))).await?;
        }
    }
    if !buf.is_empty() {
        sender
            .send(Some(TxDeltaWindow::full_text(buf.freeze())))
            .await?;
    }
    sender.send(None).await
}

//! Repository reads of the real engine: info, list, cat, save, revision diff and log.

use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use opensvn_ipc::time_fmt::timestamp_to_iso;
use opensvn_ipc::{FileDiff, LogGetRequest, LogPage, RepoEntry, RepoInfo, Rev, Size, TextSide};
use svn_ra_api::{LogRequest, RaSession};
use svn_types::{CancelToken, ErrorCode, NodeKind, RelPath, Revnum, SvnError, rename_replacing};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::text::CappedBuf;
use super::{RealEngine, wc_read};
use crate::engine::context::{OpContext, ProgressUpdate};
use crate::engine::facade::{DiffOptions, EngineResult, LogScope};
use crate::engine::mapping::{self, entry_kind, revnum};
use crate::engine::text::{file_diff, text_side};

/// Largest file `repo_cat` and revision diffs return inline.
pub(super) const CAT_LIMIT: usize = 5 * 1024 * 1024;
/// Bytes between two progress samples of a download.
const PROGRESS_STEP: u64 = 256 * 1024;

pub(super) fn internal(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::Internal, msg)
}

/// Reads a file into memory; refuses files above `limit` bytes.
pub(super) async fn read_capped(
    session: &dyn RaSession,
    path: &RelPath,
    rev: Option<Revnum>,
    limit: usize,
    cancel: &CancelToken,
) -> EngineResult<(Revnum, Vec<u8>)> {
    let mut sink = CappedBuf::new(limit);
    let meta = session.get_file(path, rev, &mut sink, cancel).await;
    if sink.exceeded() {
        return Err(internal(
            "file is too large to show; save it to disk instead",
        ));
    }
    Ok((meta?.rev, sink.into_inner()))
}

/// Writer that reports download progress while forwarding to `inner`.
struct ProgressWriter<W> {
    inner: W,
    done: u64,
    reported: u64,
    total: Option<u64>,
    ctx: OpContext,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for ProgressWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, data);
        if let Poll::Ready(Ok(n)) = &poll {
            self.done += *n as u64;
            if self.done - self.reported >= PROGRESS_STEP {
                self.reported = self.done;
                self.ctx.progress.report(ProgressUpdate {
                    phase: "network".into(),
                    bytes_done: Some(self.done),
                    bytes_total: self.total,
                    ..ProgressUpdate::default()
                });
            }
        }
        poll
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl RealEngine {
    pub(super) async fn do_repo_info(&self, url: &str, ctx: &OpContext) -> EngineResult<RepoInfo> {
        let (session, path) = self.locate(url, ctx).await?;
        let youngest = session.youngest_rev(&ctx.cancel).await?;
        let kind = match session.check_path(&path, None, &ctx.cancel).await? {
            NodeKind::Dir => opensvn_ipc::EntryKind::Dir,
            NodeKind::File => opensvn_ipc::EntryKind::File,
            _ => {
                return Err(SvnError::new(
                    ErrorCode::PathNotFound,
                    format!("{url} does not exist"),
                ));
            }
        };
        Ok(RepoInfo {
            repo_root_url: session.repos_root().to_string(),
            repo_uuid: session.uuid().to_owned(),
            youngest_rev: mapping::rev(youngest),
            kind,
        })
    }

    pub(super) async fn do_repo_list(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<Vec<RepoEntry>> {
        let (session, path) = self.locate(url, ctx).await?;
        let entries = session
            .list(
                &path,
                revnum(rev),
                svn_types::Depth::Immediates,
                &ctx.cancel,
            )
            .await?;
        let mut out: Vec<RepoEntry> = entries
            .into_iter()
            .map(|e| RepoEntry {
                kind: entry_kind(e.kind),
                size: e.size.map(Size),
                created_rev: Rev(e.created_rev.unwrap_or(Revnum(0)).0),
                last_author: e.last_author,
                date: e.time.map_or_else(String::new, timestamp_to_iso),
                lock: e.lock.map(mapping::lock),
                name: e.name,
            })
            .collect();
        out.sort_by(|a, b| {
            (
                a.kind == opensvn_ipc::EntryKind::File,
                a.name.to_lowercase(),
            )
                .cmp(&(
                    b.kind == opensvn_ipc::EntryKind::File,
                    b.name.to_lowercase(),
                ))
        });
        Ok(out)
    }

    pub(super) async fn do_repo_cat(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<TextSide> {
        let (session, path) = self.locate(url, ctx).await?;
        let (rev, data) =
            read_capped(session.as_ref(), &path, revnum(rev), CAT_LIMIT, &ctx.cancel).await?;
        Ok(text_side(
            &data,
            format!("{}@r{}", path.file_name().unwrap_or(""), rev.0),
        ))
    }

    /// Streams the file into a temporary file next to `save_to`, then renames it into place: the
    /// destination is either complete or untouched, also when the download is cancelled.
    pub(super) async fn do_repo_save(
        &self,
        url: &str,
        rev: Option<Rev>,
        save_to: &Path,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        if !save_to.is_absolute() {
            return Err(internal("save path must be absolute"));
        }
        let (parent, name) = save_to
            .parent()
            .zip(save_to.file_name())
            .ok_or_else(|| internal("save path has no file name"))?;
        let (session, path) = self.locate(url, ctx).await?;
        let total = session
            .stat(&path, revnum(rev), &ctx.cancel)
            .await?
            .and_then(|e| e.size);
        let tmp = parent.join(format!(
            ".{}.{}.part",
            name.to_string_lossy(),
            uuid::Uuid::now_v7().simple()
        ));
        let io = |what: &str, e: &std::io::Error| {
            SvnError::from_io(format!("{what} {}", tmp.display()), e)
        };
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await
            .map_err(|e| io("create", &e))?;
        let mut sink = ProgressWriter {
            inner: file,
            done: 0,
            reported: 0,
            total,
            ctx: ctx.clone(),
        };
        let download = async {
            session
                .get_file(&path, revnum(rev), &mut sink, &ctx.cancel)
                .await?;
            sink.flush().await.map_err(|e| io("write", &e))?;
            sink.inner.sync_all().await.map_err(|e| io("sync", &e))
        }
        .await;
        // Close the handle before renaming (required on Windows).
        drop(sink);
        // The last cancel point is before the rename: a file is either complete or absent.
        let outcome = download
            .and_then(|()| ctx.check())
            .and_then(|()| rename_replacing(&tmp, save_to));
        if outcome.is_err() {
            if let Err(e) = tokio::fs::remove_file(&tmp).await {
                tracing::warn!(path = %tmp.display(), error = %e, "could not remove partial download");
            }
        } else {
            ctx.progress.report(ProgressUpdate {
                phase: "network".into(),
                bytes_done: total,
                bytes_total: total,
                ..ProgressUpdate::default()
            });
        }
        outcome
    }

    pub(super) async fn do_diff_rev(
        &self,
        url: &str,
        from: Option<Rev>,
        to: Rev,
        opts: DiffOptions,
        ctx: &OpContext,
    ) -> EngineResult<FileDiff> {
        let (session, path) = self.locate(url, ctx).await?;
        let from = from.map_or(to.0.saturating_sub(1), |r| r.0);
        let name = path.file_name().unwrap_or("").to_owned();
        let read = |rev: u64| {
            let (session, path) = (session.clone(), path.clone());
            async move {
                if rev == 0 {
                    return Ok(None);
                }
                match read_capped(
                    session.as_ref(),
                    &path,
                    Some(Revnum(rev)),
                    CAT_LIMIT,
                    &ctx.cancel,
                )
                .await
                {
                    Ok((_, d)) => Ok(Some(d)),
                    // The file does not exist on that side (added or deleted by the revision).
                    Err(e) if e.code == ErrorCode::PathNotFound => Ok(None),
                    Err(e) => Err(e),
                }
            }
        };
        let left = read(from).await?.map(|d| (d, format!("{name}@r{from}")));
        let right = read(to.0).await?.map(|d| (d, format!("{name}@r{}", to.0)));
        // Same `svn_diff` display budget as the fake engine: over it, the sides are dropped and
        // `too_complex` is set unless the caller asked for full text (`diff_external`).
        // Bounded by `CAT_LIMIT` per side and the cost budget, so it runs inline like the fake.
        file_diff(&name, left, right, opts, ctx)
    }

    pub(super) async fn do_log(
        &self,
        scope: &LogScope,
        req: &LogGetRequest,
        ctx: &OpContext,
    ) -> EngineResult<LogPage> {
        let (session, path) = match scope {
            LogScope::Url(url) => self.locate(url, ctx).await?,
            LogScope::Wc { wc, path } => {
                let loc = wc_read::node_location(&wc.root, path).await?;
                self.locate_repo_path(&loc.repo_root, &loc.repos_path, ctx)
                    .await?
            }
        };
        let filter = req
            .filter
            .as_ref()
            .map(|f| f.to_lowercase())
            .filter(|f| !f.is_empty());
        let limit = req.limit.clamp(1, 1000) as usize;
        let ra = LogRequest {
            paths: vec![path],
            start: revnum(req.from_rev),
            end: revnum(req.to_rev),
            // Without a text filter the server can stop after one page plus the look-ahead entry.
            limit: filter
                .is_none()
                .then(|| u32::try_from(limit + 1).unwrap_or(u32::MAX)),
            discover_changed_paths: req.with_paths,
            strict_node_history: req.stop_on_copy.unwrap_or(false),
        };
        let mut matched = Vec::new();
        let mut page_full = false;
        let outcome = session
            .log(
                &ra,
                &mut |e| {
                    let hit = filter.as_ref().is_none_or(|f| {
                        e.message
                            .as_deref()
                            .unwrap_or("")
                            .to_lowercase()
                            .contains(f)
                            || e.author.as_deref().unwrap_or("").to_lowercase().contains(f)
                    });
                    if hit {
                        matched.push(e);
                    }
                    if matched.len() > limit {
                        // One entry beyond the page proves there is a next page; stop reading.
                        page_full = true;
                        return Err(internal("log page complete"));
                    }
                    Ok(())
                },
                &ctx.cancel,
            )
            .await;
        match outcome {
            Ok(()) => {}
            Err(_) if page_full => {}
            Err(e) => return Err(e),
        }
        let next_rev = (matched.len() > limit).then(|| mapping::rev(matched[limit].rev));
        matched.truncate(limit);
        Ok(LogPage {
            entries: matched.into_iter().map(mapping::log_entry).collect(),
            next_rev,
        })
    }
}

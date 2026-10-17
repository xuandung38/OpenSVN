//! Repository reads of the real engine: info, list, cat, save, revision diff and log.

use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use opensvn_ipc::time_fmt::timestamp_to_iso;
use opensvn_ipc::{
    FileDiff, LogGetRequest, LogPage, RepoEntry, RepoInfo, RepoListing, Rev, Size, TextSide,
};
use svn_ra_api::{DirListing, LogRequest, RaSession};
use svn_types::{CancelToken, ErrorCode, NodeKind, RelPath, Revnum, SvnError, rename_replacing};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::RealEngine;

/// A newest-first log page still incomplete after this long, with at least one entry, ends
/// with what arrived (see `do_log`).
pub(crate) const PARTIAL_LOG_AFTER: Duration = Duration::from_secs(3);

/// Entries asked for by the first request of a newest-first log page (see `do_log`).
const FIRST_LOG_REQUEST: usize = 16;

fn pager_limit(req: &opensvn_ipc::LogGetRequest) -> usize {
    req.limit.clamp(1, 1000) as usize
}
use super::text::CappedBuf;
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

/// Fills the locks of the children of `dir` from a `get-locks` request. The listing still
/// shows when the locks cannot be read (an old server, a path-based authorization rule):
/// only cancellation fails it.
async fn add_locks(
    session: &dyn RaSession,
    dir: &RelPath,
    entries: &mut [svn_types::DirEntry],
    ctx: &OpContext,
) -> EngineResult<()> {
    let locks = match session
        .get_locks(dir, svn_types::Depth::Immediates, &ctx.cancel)
        .await
    {
        Ok(locks) => locks,
        Err(err) if err.code == ErrorCode::Cancelled => return Err(err),
        Err(err) => {
            tracing::warn!(code = ?err.code, "listing without locks: {}", err.message);
            return Ok(());
        }
    };
    for lock in locks {
        let Some((parent, name)) = lock.path.as_str().rsplit_once('/') else {
            continue;
        };
        if parent.trim_start_matches('/') != dir.as_str() {
            continue;
        }
        if let Some(entry) = entries.iter_mut().find(|e| e.name == name) {
            entry.lock = Some(lock);
        }
    }
    Ok(())
}

impl RealEngine {
    pub(super) async fn do_repo_info(
        &self,
        url: &str,
        peg: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<RepoInfo> {
        let (session, path) = self.locate(url, ctx).await?;
        let youngest = session.youngest_rev(&ctx.cancel).await?;
        let kind = match session.check_path(&path, revnum(peg), &ctx.cancel).await? {
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
    ) -> EngineResult<RepoListing> {
        let (session, path) = self.locate(url, ctx).await?;
        // Children with names the app refuses are left out by the session; only their number
        // comes back, so Browse can say they exist instead of showing an empty folder.
        let DirListing {
            mut entries,
            rejected,
        } = session
            .list_counted(
                &path,
                revnum(rev),
                svn_types::Depth::Immediates,
                &ctx.cancel,
            )
            .await?;
        // A DAV listing carries no locks (`svn ls` asks for them separately too). Locks only
        // exist in HEAD, so they are added to a HEAD listing only.
        if rev.is_none() && !entries.is_empty() && entries.iter().all(|e| e.lock.is_none()) {
            add_locks(session.as_ref(), &path, &mut entries, ctx).await?;
        }
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
        Ok(RepoListing {
            entries: out,
            hidden: u32::try_from(rejected).unwrap_or(u32::MAX),
        })
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
            LogScope::Repo(loc) => {
                self.locate_repo_path(&loc.repo_root, &RelPath::new(&loc.repos_path)?, ctx)
                    .await?
            }
        };
        let mut pager = mapping::LogPager::new(req);
        let mut ra = LogRequest {
            paths: vec![path],
            start: revnum(req.from_rev),
            end: revnum(req.to_rev),
            limit: None,
            discover_changed_paths: req.with_paths,
            strict_node_history: req.stop_on_copy.unwrap_or(false),
            include_merged_revisions: req.include_merged.unwrap_or(false),
            revprops: mapping::log_revprops(req),
        };
        let page_full = AtomicBool::new(false);
        let taken = AtomicUsize::new(0);
        // Behind authz the server checks every changed path of each revision it reports, and
        // it sends nothing before its output buffer fills: a page that reaches a revision with
        // many changed paths (often the import at r1) shows nothing for minutes. So a
        // newest-first page is read in requests of growing size (16, 32, ... entries), and a
        // page still incomplete after `PARTIAL_LOG_AFTER` ends with what arrived (the next page
        // reads on from there). With `strict_node_history` each request goes on below the last
        // entry of the one before. Otherwise it cannot: the server looks the path up at the
        // start revision, where a copied or renamed node did not exist yet. The same range is
        // then read again with a larger limit, and the entries already taken are skipped.
        let newest_first = match (req.from_rev, req.to_rev) {
            (Some(from), Some(to)) => from >= to,
            _ => true,
        };
        // A text filter is applied here, not by the server: one request reads the range.
        let filtered = pager.filtered();
        let probing = newest_first && !filtered;
        let floor = req.to_rev.map_or(1, |r| r.0);
        let last_own = AtomicU64::new(0);
        let moves_on = ra.strict_node_history;
        // Own entries of a range read again still to skip, and whether the merged revisions
        // that follow belong to a skipped one.
        let skip = AtomicUsize::new(0);
        let skipping = AtomicBool::new(false);
        let mut on_entry = |e: svn_types::LogEntry| {
            if e.merge_depth == 0 {
                let left = skip.load(Ordering::Relaxed);
                skipping.store(left > 0, Ordering::Relaxed);
                if left > 0 {
                    skip.store(left - 1, Ordering::Relaxed);
                    return Ok(());
                }
            } else if skipping.load(Ordering::Relaxed) {
                return Ok(());
            }
            let own = (e.merge_depth == 0).then_some(e.rev.0);
            let more = pager.accept(e);
            if let Some(rev) = own.filter(|_| more) {
                last_own.store(rev, Ordering::Relaxed);
            }
            taken.store(pager.taken(), Ordering::Relaxed);
            if more {
                return Ok(());
            }
            // One entry beyond the page proves there is a next page; stop reading.
            page_full.store(true, Ordering::Relaxed);
            Err(internal("log page complete"))
        };
        let mut check = tokio::time::interval_at(
            tokio::time::Instant::now() + PARTIAL_LOG_AFTER,
            Duration::from_millis(250),
        );
        let mut size = FIRST_LOG_REQUEST;
        let mut cut = false;
        loop {
            let before = taken.load(Ordering::Relaxed);
            // The page plus the look-ahead entry, or less while probing.
            let wanted = pager_limit(req) + 1 - before;
            let asked = if probing { wanted.min(size) } else { wanted };
            // Read again from the start: the entries already taken come first.
            let again = if moves_on { 0 } else { before };
            skip.store(again, Ordering::Relaxed);
            ra.limit = (!filtered).then(|| u32::try_from(again + asked).unwrap_or(u32::MAX));
            let mut read = Box::pin(session.log(&ra, &mut on_entry, &ctx.cancel));
            let outcome = loop {
                tokio::select! {
                    biased;
                    // A finished read wins over the clock.
                    r = &mut read => break Some(r),
                    _ = check.tick(), if newest_first => {
                        if taken.load(Ordering::Relaxed) > 0 {
                            break None;
                        }
                    }
                }
            };
            drop(read);
            match outcome {
                None => {
                    cut = true;
                    break;
                }
                Some(Ok(())) => {}
                Some(Err(_)) if page_full.load(Ordering::Relaxed) => break,
                Some(Err(e)) => return Err(e),
            }
            let got = taken.load(Ordering::Relaxed) - before;
            // Fewer entries than asked for: the range is exhausted.
            if !probing || got < asked {
                break;
            }
            let last = last_own.load(Ordering::Relaxed);
            if last <= floor {
                break;
            }
            if moves_on {
                ra.start = Some(Revnum(last - 1));
            }
            size = size.saturating_mul(2);
        }
        if cut {
            pager.cut(floor);
        }
        Ok(pager.page())
    }
}

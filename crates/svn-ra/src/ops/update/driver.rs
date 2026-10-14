//! Update, switch and status drive: report in, editor calls out.

use svn_protocol::ResponseParser;
use svn_protocol::update::{
    UpdateMode as WireMode, UpdateParser, UpdateReportBuilder, UpdateRequestOptions,
};
use svn_ra_api::{
    DEFAULT_COMMAND_CAPACITY, EditorHandle, EditorWorker, ReportFn, UpdateMode, UpdateRequest,
};
use svn_types::{
    CancelToken, DeltaEditor, Depth, ErrorCode, RelPath, RepoUrl, Reporter, Result, Revnum,
    SvnError,
};

use super::event_bridge::EventBridge;
use super::fetch_scheduler::FetchScheduler;
use super::wire::RaWire;

/// Collects the working copy's reporter calls into a report request.
struct CollectingReporter {
    wire: RaWire,
    builder: UpdateReportBuilder,
    finished: bool,
    aborted: bool,
    /// The update target: the caller reports session-relative paths, the wire wants them
    /// relative to the target (`""` is the target itself), like svn's reporter.
    target: RelPath,
}

impl CollectingReporter {
    fn wire_path(&self, path: &RelPath) -> Result<RelPath> {
        if self.target.is_root() {
            return Ok(path.clone());
        }
        if path == &self.target {
            return Ok(RelPath::root());
        }
        path.as_str()
            .strip_prefix(self.target.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
            .map(RelPath::new)
            .transpose()?
            .ok_or_else(|| {
                SvnError::new(
                    ErrorCode::Internal,
                    format!(
                        "report path {path} lies outside the update target {}",
                        self.target
                    ),
                )
            })
    }
}

impl Reporter for CollectingReporter {
    fn set_path(
        &mut self,
        path: &RelPath,
        rev: Revnum,
        depth: Depth,
        start_empty: bool,
        lock_token: Option<&str>,
    ) -> Result<()> {
        let path = self.wire_path(path)?;
        self.builder
            .set_path(&path, rev, depth, start_empty, lock_token);
        Ok(())
    }

    fn delete_path(&mut self, path: &RelPath) -> Result<()> {
        let path = self.wire_path(path)?;
        self.builder.delete_path(&path);
        Ok(())
    }

    fn link_path(
        &mut self,
        path: &RelPath,
        url: &RepoUrl,
        rev: Revnum,
        depth: Depth,
        start_empty: bool,
        lock_token: Option<&str>,
    ) -> Result<()> {
        let link = self.wire.repo_path_of_url(url.as_url().as_str())?;
        let path = self.wire_path(path)?;
        self.builder
            .link_path(&path, &link, rev, depth, start_empty, lock_token);
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.finished = true;
        Ok(())
    }

    fn abort(&mut self) -> Result<()> {
        self.aborted = true;
        Ok(())
    }
}

/// Runs the report function on a blocking thread and returns the collected report.
async fn collect_report(
    wire: &RaWire,
    opts: UpdateRequestOptions,
    target: RelPath,
    report: ReportFn,
) -> Result<UpdateReportBuilder> {
    let wire = wire.clone();
    let joined = tokio::task::spawn_blocking(move || {
        let mut reporter = CollectingReporter {
            wire,
            builder: UpdateReportBuilder::new(opts),
            finished: false,
            aborted: false,
            target,
        };
        report(&mut reporter)?;
        if reporter.aborted {
            return Err(SvnError::new(
                ErrorCode::Cancelled,
                "the report was aborted",
            ));
        }
        if !reporter.finished {
            return Err(SvnError::new(
                ErrorCode::Internal,
                "the report function returned without finishing the report",
            ));
        }
        Ok(reporter.builder)
    })
    .await;
    joined.map_err(|e| SvnError::new(ErrorCode::Internal, format!("report task failed: {e}")))?
}

/// Request options for `req`, choosing send-all or skeleton mode from the server capabilities.
fn request_options(wire: &RaWire, req: &UpdateRequest) -> Result<UpdateRequestOptions> {
    let caps = wire.caps();
    let mut mode = WireMode::select(caps, wire.bulk_preference());
    if matches!(mode, WireMode::Skelta { .. }) && wire.connections() < 2 {
        // Downloads need a connection besides the one held by the report.
        if caps.bulk_updates_allowed() {
            mode = WireMode::SendAll;
        } else {
            return Err(SvnError::new(
                ErrorCode::ServerUnsupported,
                "the server forbids bulk updates and the connection limit leaves no \
                 connection for parallel downloads",
            ));
        }
    }
    // Like svn, the edit is anchored at the target's parent and names the target by its last
    // component, so a nested target (`a/b/c`) works on a session anchored above it.
    let (anchor, name) = split_target(&req.target);
    let mut opts = UpdateRequestOptions::new(mode, wire.repo_path(&anchor)?);
    opts.target_revision = req.revision;
    opts.depth = req.depth;
    opts.ignore_ancestry = req.ignore_ancestry;
    opts.send_copyfrom_args = req.send_copyfrom;
    opts.text_deltas = req.mode != UpdateMode::Status;
    if let UpdateMode::Switch(url) = &req.mode {
        opts.dst = Some(wire.repo_path_of_url(url.as_url().as_str())?);
    }
    opts.update_target = name;
    Ok(opts)
}

/// `(anchor, last component)` of a session-relative update target; the root is its own anchor.
fn split_target(target: &RelPath) -> (RelPath, Option<String>) {
    match (target.parent(), target.file_name()) {
        (Some(parent), Some(name)) if !target.is_root() => (parent, Some(name.to_owned())),
        _ => (RelPath::root(), None),
    }
}

async fn drive(
    wire: &RaWire,
    req: UpdateRequest,
    report: ReportFn,
    editor: &EditorHandle,
    cancel: &CancelToken,
) -> Result<()> {
    cancel.check()?;
    let opts = request_options(wire, &req)?;
    let send_text = opts.text_deltas;
    let skelta = matches!(opts.mode, WireMode::Skelta { .. });
    let builder = collect_report(wire, opts, req.target.clone(), report).await?;
    let request = builder.build(wire.urls())?;
    let resp = wire.send_ok(request, cancel).await?;
    let mut resp = resp;
    let scheduler = skelta.then(|| FetchScheduler::new(wire, cancel));
    let mut bridge = EventBridge::new(editor, wire, scheduler, send_text, cancel)
        .rooted_at(split_target(&req.target).0);
    let mut parser = UpdateParser::new(wire.urls().clone());
    while let Some(chunk) = resp.chunk().await? {
        for event in parser.feed(&chunk)? {
            bridge.handle(event).await?;
        }
    }
    for event in parser.finish()? {
        bridge.handle(event).await?;
    }
    bridge.finish().await
}

/// Drives `editor` with the changes the server reports for `req`.
///
/// The editor runs on a blocking thread behind an [`EditorWorker`]. On any error, including
/// cancellation, the editor receives `abort_edit` and the original error is returned.
pub async fn run_update(
    wire: &RaWire,
    req: UpdateRequest,
    report: ReportFn,
    editor: Box<dyn DeltaEditor>,
    cancel: &CancelToken,
) -> Result<()> {
    let handle = EditorWorker::spawn_pipelined(editor, DEFAULT_COMMAND_CAPACITY);
    match drive(wire, req, report, &handle, cancel).await {
        Ok(()) => handle.close_edit().await,
        Err(e) => {
            // The abort result adds nothing to the failure that caused it.
            let _ = handle.abort_edit().await;
            Err(e)
        }
    }
}

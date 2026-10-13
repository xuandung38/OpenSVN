//! Remote operation: log. One `log-report` REPORT, parsed as it streams in.
//!
//! Each entry is handed to the caller as soon as it is complete, so memory stays flat for any
//! history length and a slow consumer slows the download down. Both ends of the range are sent
//! explicitly: mod_dav_svn reads a missing end revision as HEAD.

use svn_protocol::log::{LogParser, LogRequest as WireLogRequest, LogRevprops, log_request};
use svn_ra_api::LogRequest;
use svn_types::{CancelToken, LogEntry, Result, Revnum};

use super::read::{peg, stream_items};
use super::update::RaWire;

/// Streams the log entries of `req` (newest first for a descending range) into `on_entry`.
pub(crate) async fn log(
    wire: &RaWire,
    req: &LogRequest,
    on_entry: &mut (dyn FnMut(LogEntry) -> Result<()> + Send),
    cancel: &CancelToken,
) -> Result<()> {
    let start = peg(wire, req.start, cancel).await?;
    // Like `svn log` (and the fake session), an open end stops at r1: r0 never has changes.
    let end = req.end.unwrap_or(Revnum(1.min(start.0)));
    // libsvn_ra_serf addresses the newer end of the range so that paths are looked up where
    // they exist.
    let peg_rev = start.max(end);
    let opts = WireLogRequest {
        start: Some(start),
        end: Some(end),
        limit: req.limit,
        discover_changed_paths: req.discover_changed_paths,
        strict_node_history: req.strict_node_history,
        // TODO(ra-api): `svn_ra_api::LogRequest` has no fields for merged revisions (`-g`) or a
        // revision-property selection yet; add them there and pass them through here.
        include_merged_revisions: false,
        revprops: LogRevprops::Default,
        paths: req.paths.clone(),
    };
    let http = log_request(wire.urls(), peg_rev, wire.session_rel(), &opts)?;
    let resp = wire.send_ok(http, cancel).await?;
    let mut parser = LogParser::new();
    stream_items(resp, &mut parser, cancel, |item| {
        cancel.check()?;
        on_entry(item.entry)
    })
    .await
}

//! Remote operation: log. One `log-report` REPORT, parsed as it streams in.
//!
//! Each entry is handed to the caller as soon as it is complete, so memory stays flat for any
//! history length and a slow consumer slows the download down. Both ends of the range are sent
//! explicitly: mod_dav_svn reads a missing end revision as HEAD.

use svn_protocol::log::{LogParser, LogRequest as WireLogRequest, LogRevprops, log_request};
use svn_ra_api::LogRequest;
use svn_types::{CancelToken, LogEntry, RelPath, Result, Revnum};

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
    // Like libsvn_ra_serf (whose session sits at the target), the REPORT goes to the deepest
    // common ancestor of the paths, not to the session root: under path-based authorization the
    // user may be allowed on `/trunk` but not on the repository root the session is rooted at.
    let (anchor, paths) = anchor_paths(&req.paths)?;
    let node = join(wire.session_rel(), &anchor)?;
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
        paths,
    };
    let http = log_request(wire.urls(), peg_rev, &node, &opts)?;
    let resp = wire.send_ok(http, cancel).await?;
    let mut parser = LogParser::new();
    stream_items(resp, &mut parser, cancel, |item| {
        cancel.check()?;
        on_entry(item.entry)
    })
    .await
}

/// Deepest common ancestor of `paths` and the paths relative to it; no paths when they all are
/// the ancestor itself (the report then names the anchor alone).
fn anchor_paths(paths: &[RelPath]) -> Result<(RelPath, Vec<RelPath>)> {
    let Some((first, rest)) = paths.split_first() else {
        return Ok((RelPath::root(), Vec::new()));
    };
    let mut common: Vec<&str> = first.components().collect();
    for p in rest {
        let same = common
            .iter()
            .zip(p.components())
            .take_while(|(a, b)| **a == *b)
            .count();
        common.truncate(same);
    }
    let anchor = RelPath::new(&common.join("/"))?;
    let skip = common.len();
    let relative = paths
        .iter()
        .map(|p| RelPath::new(&p.components().skip(skip).collect::<Vec<_>>().join("/")))
        .collect::<Result<Vec<_>>>()?;
    let relative = if relative.iter().all(RelPath::is_root) {
        Vec::new()
    } else {
        relative
    };
    Ok((anchor, relative))
}

fn join(base: &RelPath, rel: &RelPath) -> Result<RelPath> {
    match (base.is_root(), rel.is_root()) {
        (_, true) => Ok(base.clone()),
        (true, false) => Ok(rel.clone()),
        (false, false) => RelPath::new(&format!("{base}/{rel}")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn p(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    #[test]
    fn report_is_anchored_at_the_common_ancestor_of_the_paths() {
        assert_eq!(anchor_paths(&[]).unwrap(), (RelPath::root(), vec![]));
        assert_eq!(anchor_paths(&[p("trunk")]).unwrap(), (p("trunk"), vec![]));
        assert_eq!(
            anchor_paths(&[p("trunk/src/a.rs"), p("trunk/README.md")]).unwrap(),
            (p("trunk"), vec![p("src/a.rs"), p("README.md")])
        );
        assert_eq!(
            anchor_paths(&[p("trunk/a"), p("trunk")]).unwrap(),
            (p("trunk"), vec![p("a"), RelPath::root()])
        );
        // Components, not string prefixes.
        assert_eq!(
            anchor_paths(&[p("trunk"), p("trunk2")]).unwrap(),
            (RelPath::root(), vec![p("trunk"), p("trunk2")])
        );
        assert_eq!(join(&p("trunk"), &p("src")).unwrap(), p("trunk/src"));
        assert_eq!(join(&RelPath::root(), &p("src")).unwrap(), p("src"));
        assert_eq!(join(&p("trunk"), &RelPath::root()).unwrap(), p("trunk"));
    }
}

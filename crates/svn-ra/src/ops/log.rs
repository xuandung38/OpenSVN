//! Remote operation: log. One `log-report` REPORT, parsed as it streams in.
//!
//! Each entry is handed to the caller as soon as it is complete, so memory stays flat for any
//! history length and a slow consumer slows the download down. Both ends of the range are sent
//! explicitly: mod_dav_svn reads a missing end revision as HEAD.

use svn_protocol::log::{LogParser, LogRequest as WireLogRequest, LogRevprops, log_request};
use svn_ra_api::LogRequest;
use svn_types::{CancelToken, LogEntry, RelPath, Result, Revnum};

use super::read::{peg, stream_items};
use super::resume::ReadRetry;
use super::update::RaWire;

/// Streams the log entries of `req` (newest first for a descending range) into `on_entry`.
///
/// When the connection fails mid-report, the report is asked again and continues after the last
/// entry handed over (see [`super::resume`]); `on_entry` never sees an entry twice:
///
/// - ascending range: from the next revision on, at the same peg;
/// - descending range with `strict_node_history` and at most one path: from the last revision
///   handed over (the path exists there under the same name, since copies are not followed);
/// - any other descending range: the original report again, skipping the entries already
///   handed over (with copies followed, the path may have had another name at older revisions,
///   so the start cannot move).
pub(crate) async fn log(
    wire: &RaWire,
    req: &LogRequest,
    on_entry: &mut (dyn FnMut(LogEntry) -> Result<()> + Send),
    cancel: &CancelToken,
) -> Result<()> {
    let start = peg(wire, req.start, cancel).await?;
    // Like `svn log` (and the fake session), an open end stops at r1: r0 never has changes.
    let end = req.end.unwrap_or(Revnum(1.min(start.0)));
    // Like libsvn_ra_serf (whose session sits at the target), the REPORT goes to the deepest
    // common ancestor of the paths, not to the session root: under path-based authorization the
    // user may be allowed on `/trunk` but not on the repository root the session is rooted at.
    let (anchor, paths) = anchor_paths(&req.paths)?;
    let node = join(wire.session_rel(), &anchor)?;
    let mut progress = LogProgress::new(start, end, req);
    let mut retry = ReadRetry::new(wire, "log");
    loop {
        let Some((from, limit)) = progress.next_request() else {
            // The broken report had already handed over everything it could contain.
            return Ok(());
        };
        let opts = WireLogRequest {
            start: Some(from),
            end: Some(end),
            limit,
            discover_changed_paths: req.discover_changed_paths,
            strict_node_history: req.strict_node_history,
            // TODO(ra-api): `svn_ra_api::LogRequest` has no fields for merged revisions (`-g`)
            // or a revision-property selection yet; add them there and pass them through here.
            include_merged_revisions: false,
            revprops: LogRevprops::Default,
            paths: paths.clone(),
        };
        // libsvn_ra_serf addresses the newer end of the range so that paths are looked up where
        // they exist.
        let http = log_request(wire.urls(), from.max(end), &node, &opts)?;
        let before = progress.delivered;
        let mut consumer_failed = false;
        let attempt = async {
            let resp = wire.send_ok(http, cancel).await?;
            let mut parser = LogParser::new();
            stream_items(resp, &mut parser, cancel, |item| {
                cancel.check()?;
                if !progress.accept(&item.entry) {
                    return Ok(());
                }
                on_entry(item.entry).inspect_err(|_| consumer_failed = true)
            })
            .await
        }
        .await;
        let err = match attempt {
            Ok(()) => return Ok(()),
            // The consumer's own failure (or its way to stop early) is never retried.
            Err(e) if consumer_failed => return Err(e),
            Err(e) => e,
        };
        if progress.delivered > before {
            retry.progressed();
        }
        retry.again(err, cancel).await?;
    }
}

/// What a log report handed over so far, and where a broken one resumes.
struct LogProgress {
    start: Revnum,
    end: Revnum,
    /// The requested limit; `None` (or 0, which the server reads the same way) is unlimited.
    limit: Option<u32>,
    /// The limit as asked, for the first request.
    asked_limit: Option<u32>,
    /// The start may move to the last entry on resume (see [`log`]).
    movable_start: bool,
    delivered: u32,
    last: Option<Revnum>,
}

impl LogProgress {
    fn new(start: Revnum, end: Revnum, req: &LogRequest) -> Self {
        Self {
            start,
            end,
            limit: req.limit.filter(|l| *l > 0),
            asked_limit: req.limit,
            movable_start: req.strict_node_history && req.paths.len() <= 1,
            delivered: 0,
            last: None,
        }
    }

    fn ascending(&self) -> bool {
        self.start < self.end
    }

    /// Start revision and limit of the next request; `None` when nothing can be left.
    fn next_request(&self) -> Option<(Revnum, Option<u32>)> {
        let Some(last) = self.last else {
            return Some((self.start, self.asked_limit));
        };
        let remaining = match self.limit {
            Some(l) if self.delivered >= l => return None,
            Some(l) => Some(l - self.delivered),
            None => None,
        };
        if last == self.end {
            return None;
        }
        if self.ascending() {
            Some((Revnum(last.0 + 1), remaining))
        } else if self.movable_start {
            // `last` comes again (and is skipped): one more entry.
            Some((last, remaining.map(|r| r.saturating_add(1))))
        } else {
            Some((self.start, self.limit))
        }
    }

    /// Whether `entry` is new (and is then counted as handed over): entries at or before the
    /// last one handed over, and entries beyond the limit, are dropped.
    fn accept(&mut self, entry: &LogEntry) -> bool {
        if let Some(last) = self.last {
            let seen = if self.ascending() {
                entry.rev <= last
            } else {
                entry.rev >= last
            };
            if seen {
                return false;
            }
        }
        if self.limit.is_some_and(|l| self.delivered >= l) {
            return false;
        }
        self.delivered += 1;
        self.last = Some(entry.rev);
        true
    }
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

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
            include_merged_revisions: req.include_merged_revisions,
            revprops: wire_revprops(&req.revprops),
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
                if !progress.accept(&item.entry, item.merge_depth) {
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

/// The wire form of a revision-property selection.
fn wire_revprops(r: &svn_ra_api::LogRevprops) -> LogRevprops {
    match r {
        svn_ra_api::LogRevprops::Default => LogRevprops::Default,
        svn_ra_api::LogRevprops::All => LogRevprops::All,
        svn_ra_api::LogRevprops::None => LogRevprops::None,
        svn_ra_api::LogRevprops::Only(names) => LogRevprops::Only(names.clone()),
    }
}

/// How to treat the children of the top-level entry currently being replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildrenSkip {
    /// Deliver every child normally.
    None,
    /// Skip every child: this top-level entry was already fully handed over on an earlier pass.
    All,
    /// Skip this many more children (catching up to a merge group a broken connection left
    /// half-delivered), then deliver the rest normally.
    Count(u32),
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
    /// The last top-level entry (`merge_depth == 0`) handed over. `-g` nests the merged-in
    /// revisions under the commit that merged them, at whatever depth and order the server
    /// built, with no relation to this range's direction: only top-level entries move this on
    /// and count against `limit`.
    last: Option<Revnum>,
    /// Whether `last` had `has-children`: a broken connection may then have delivered only
    /// some of its merged-in revisions, so an ascending resume must replay from `last` again
    /// rather than skip past the revisions it merged in.
    last_has_children: bool,
    /// Children of `last` handed over so far, across every pass (including ones broken off).
    children_delivered: u32,
    skip_children: ChildrenSkip,
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
            last_has_children: false,
            children_delivered: 0,
            skip_children: ChildrenSkip::None,
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
            if self.last_has_children {
                // `last` may have merged-in revisions the consumer never saw: replay from
                // `last` again instead of skipping past them. `last` itself is filtered out by
                // `accept` (it is already delivered), so it wastes one of the server's `limit`
                // slots without yielding a new top-level entry: ask for one more, same as the
                // `movable_start` resend below.
                Some((last, remaining.map(|r| r.saturating_add(1))))
            } else {
                Some((Revnum(last.0 + 1), remaining))
            }
        } else if self.movable_start {
            // `last` comes again (and is skipped): one more entry.
            Some((last, remaining.map(|r| r.saturating_add(1))))
        } else {
            Some((self.start, self.limit))
        }
    }

    /// Whether `entry` at `merge_depth` is new and should reach the consumer.
    ///
    /// Only top-level entries (`merge_depth == 0`) move `last` and count against `limit`: `-g`
    /// nests an arbitrary number of merged-in revisions, at an arbitrary depth and in no
    /// particular order relative to the range's direction, under the commit that merged them.
    /// A child reaches the consumer unless it belongs to a top-level entry already fully handed
    /// over on an earlier pass; `children_delivered` tracks how many of the current group
    /// already went through, so a connection that broke mid-group resumes exactly where it left
    /// off, without losing or repeating any child.
    fn accept(&mut self, entry: &LogEntry, merge_depth: u32) -> bool {
        if merge_depth == 0 {
            if let Some(last) = self.last {
                if entry.rev == last {
                    // The boundary entry of a resumed pass: already handed over, but its
                    // children may not all be.
                    self.skip_children = ChildrenSkip::Count(self.children_delivered);
                    return false;
                }
                let already_seen = if self.ascending() {
                    entry.rev < last
                } else {
                    entry.rev > last
                };
                if already_seen {
                    self.skip_children = ChildrenSkip::All;
                    return false;
                }
            }
            if self.limit.is_some_and(|l| self.delivered >= l) {
                self.skip_children = ChildrenSkip::All;
                return false;
            }
            self.delivered += 1;
            self.last = Some(entry.rev);
            self.last_has_children = entry.has_children;
            self.children_delivered = 0;
            self.skip_children = ChildrenSkip::None;
            return true;
        }
        match self.skip_children {
            ChildrenSkip::All => false,
            ChildrenSkip::Count(0) | ChildrenSkip::None => {
                self.skip_children = ChildrenSkip::None;
                self.children_delivered += 1;
                true
            }
            ChildrenSkip::Count(remaining) => {
                self.skip_children = ChildrenSkip::Count(remaining - 1);
                false
            }
        }
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

    fn entry(rev: u64, has_children: bool) -> LogEntry {
        LogEntry {
            rev: Revnum(rev),
            author: None,
            date: None,
            message: None,
            changed_paths: Vec::new(),
            has_children,
            merge_depth: 0,
            subtractive_merge: false,
            revprops: Vec::new(),
        }
    }

    fn request(limit: Option<u32>, strict_node_history: bool, paths: Vec<RelPath>) -> LogRequest {
        LogRequest {
            paths,
            start: None,
            end: None,
            limit,
            discover_changed_paths: false,
            strict_node_history,
            ..Default::default()
        }
    }

    fn progress(start: u64, end: u64, limit: Option<u32>) -> LogProgress {
        LogProgress::new(
            Revnum(start),
            Revnum(end),
            &request(limit, false, Vec::new()),
        )
    }

    /// Drives `accept` over `items` (`(rev, merge_depth, has_children)`), returning the
    /// revisions handed over, in order.
    fn deliver(p: &mut LogProgress, items: &[(u64, u32, bool)]) -> Vec<u64> {
        items
            .iter()
            .filter_map(|&(rev, depth, has_children)| {
                p.accept(&entry(rev, has_children), depth).then_some(rev)
            })
            .collect()
    }

    #[test]
    fn descending_without_merges_matches_the_old_behaviour() {
        let mut p = progress(10, 1, None);
        assert_eq!(
            deliver(&mut p, &[(10, 0, false), (9, 0, false), (8, 0, false)]),
            [10, 9, 8]
        );
        // A broken connection without `movable_start` replays the whole range again; already
        // delivered revisions must not reach the consumer twice.
        assert_eq!(p.next_request(), Some((Revnum(10), None)));
        assert_eq!(
            deliver(
                &mut p,
                &[(10, 0, false), (9, 0, false), (8, 0, false), (7, 0, false)]
            ),
            [7]
        );
    }

    #[test]
    fn ascending_without_merges_matches_the_old_behaviour() {
        let mut p = progress(1, 10, None);
        assert_eq!(
            deliver(&mut p, &[(1, 0, false), (2, 0, false), (3, 0, false)]),
            [1, 2, 3]
        );
        assert_eq!(p.next_request(), Some((Revnum(4), None)));
        assert_eq!(deliver(&mut p, &[(4, 0, false)]), [4]);
    }

    #[test]
    fn movable_start_resumes_at_the_last_entry_again() {
        let mut p = LogProgress::new(
            Revnum(10),
            Revnum(1),
            &request(None, true, vec![p("trunk")]),
        );
        assert_eq!(deliver(&mut p, &[(10, 0, false), (9, 0, false)]), [10, 9]);
        assert_eq!(p.next_request(), Some((Revnum(9), None)));
        // `last` (9) comes again and is skipped, not delivered twice.
        assert_eq!(deliver(&mut p, &[(9, 0, false), (8, 0, false)]), [8]);
    }

    #[test]
    fn one_level_merge_children_are_delivered_without_moving_last() {
        let mut p = progress(11, 1, None);
        let revs = deliver(
            &mut p,
            &[(11, 0, true), (10, 1, false), (9, 1, false), (7, 0, false)],
        );
        assert_eq!(revs, [11, 10, 9, 7]);
    }

    #[test]
    fn nested_two_and_three_level_merges_are_all_delivered() {
        let mut p = progress(11, 1, None);
        let revs = deliver(
            &mut p,
            &[
                (11, 0, true),
                (10, 1, true),
                (9, 2, true),
                (8, 3, false),
                (6, 2, false),
                (5, 1, false),
                (4, 0, false),
            ],
        );
        assert_eq!(revs, [11, 10, 9, 8, 6, 5, 4]);
    }

    #[test]
    fn limit_counts_only_top_level_entries() {
        // Matches `svn log -g --limit N` (svn 1.15.0): a merge's children are never counted
        // against the limit and are always delivered in full once their top-level entry is.
        let mut p = progress(11, 1, Some(1));
        let revs = deliver(
            &mut p,
            &[
                (11, 0, true),
                (10, 1, false),
                (9, 1, false),
                (7, 0, true),
                (6, 1, false),
            ],
        );
        assert_eq!(revs, [11, 10, 9]);
    }

    #[test]
    fn disconnect_mid_children_group_resumes_without_loss_or_duplication() {
        // Descending, non-movable: a broken connection replays the whole range from `self.start`.
        let mut p = progress(11, 1, None);
        let full = [
            (11, 0, true),
            (10, 1, false),
            (9, 1, false),
            (8, 1, false),
            (7, 0, false),
        ];
        // Only the top entry and its first child reach the consumer before the connection
        // breaks mid-group.
        assert_eq!(deliver(&mut p, &full[..2]), [11, 10]);
        assert_eq!(p.next_request(), Some((Revnum(11), None)));
        // The resumed pass replays everything again; the boundary top and its first child are
        // skipped, the rest reaches the consumer exactly once.
        assert_eq!(deliver(&mut p, &full), [9, 8, 7]);
    }

    #[test]
    fn ascending_disconnect_mid_children_group_resumes_from_the_top_entry_again() {
        let mut p = progress(1, 11, None);
        let full = [
            (1, 0, false),
            (7, 0, true),
            (8, 1, false),
            (9, 1, false),
            (10, 1, false),
            (11, 0, false),
        ];
        assert_eq!(deliver(&mut p, &full[..3]), [1, 7, 8]);
        // `last` (7) has children: resuming must replay from 7 again, not jump to 8, or the
        // children not yet delivered would never be asked for again.
        assert_eq!(p.next_request(), Some((Revnum(7), None)));
        assert_eq!(deliver(&mut p, &full), [9, 10, 11]);
    }

    #[test]
    fn ascending_limit_resend_of_last_with_children_accounts_for_the_wasted_slot() {
        // `last` (7) is resent to catch up on its merge children; `next_request` must ask the
        // server for one more than the limit still owes, or the resend itself eats a slot and
        // the next genuinely new top-level entry never arrives (one fewer top-level entry than
        // the consumer is actually owed).
        let mut p = progress(1, 11, Some(3));
        assert_eq!(
            deliver(
                &mut p,
                &[(1, 0, false), (7, 0, true), (8, 1, false), (9, 1, false)]
            ),
            [1, 7, 8, 9]
        );
        // One top-level entry still owed (3 delivered minus 2 so far): the budget sent to the
        // server must be 2, not 1, to cover the wasted resend of 7.
        assert_eq!(p.next_request(), Some((Revnum(7), Some(2))));
        // The server honours that budget: it resends 7's full group (now complete, 8 and 9
        // skipped as already seen) plus exactly one more top-level entry.
        assert_eq!(
            deliver(
                &mut p,
                &[
                    (7, 0, true),
                    (8, 1, false),
                    (9, 1, false),
                    (10, 1, false),
                    (11, 0, false),
                ],
            ),
            [10, 11]
        );
        assert_eq!(p.delivered, 3);
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

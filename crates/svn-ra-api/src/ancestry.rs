//! Common ancestry of two repository locations, for the check `svn switch` makes before it
//! re-points a working copy item (`svn_client__get_youngest_common_ancestor`).
//!
//! Each location's history is read as location segments and compared like svn compares the
//! "history as mergeinfo": the two share an ancestor when one path appears in both histories in
//! at least one common revision above r0. Two histories that meet only in r0 (both start at the
//! repository root) share the root at r0.

use crate::{LocationSegment, RaSession};
use svn_types::{CancelToken, RelPath, Result, Revnum};

/// A location's history: `(path, oldest, youngest)` per segment, gaps left out, and whether it
/// reaches back to r0.
struct History {
    ranges: Vec<(String, u64, u64)>,
    has_rev_zero: bool,
}

impl History {
    fn from_segments(segments: &[LocationSegment]) -> Self {
        let has_rev_zero = segments
            .iter()
            .min_by_key(|s| s.range_start.0)
            .is_some_and(|s| s.range_start.0 == 0);
        // svn's mergeinfo ranges are `(start - 1, end]`: r0 itself never counts.
        let ranges = segments
            .iter()
            .filter_map(|s| {
                let path = s.path.as_ref()?.as_str().to_owned();
                let start = s.range_start.0.max(1);
                (start <= s.range_end.0).then_some((path, start, s.range_end.0))
            })
            .collect();
        Self {
            ranges,
            has_rev_zero,
        }
    }
}

/// The youngest common ancestor of the two histories: `(absolute path, revision)`, or `None`.
fn youngest_common(a: &History, b: &History) -> Option<(String, u64)> {
    let mut best: Option<(String, u64)> = None;
    for (pa, sa, ea) in &a.ranges {
        for (pb, sb, eb) in &b.ranges {
            if pa != pb {
                continue;
            }
            let (start, end) = ((*sa).max(*sb), (*ea).min(*eb));
            if start <= end && best.as_ref().is_none_or(|(_, r)| end > *r) {
                best = Some((pa.clone(), end));
            }
        }
    }
    best.or_else(|| (a.has_rev_zero && b.has_rev_zero).then(|| ("/".to_owned(), 0)))
}

/// Whether `a` and `b` (repository-relative paths at their revisions) share any history.
pub async fn have_common_ancestor(
    session: &dyn RaSession,
    a: (&RelPath, Revnum),
    b: (&RelPath, Revnum),
    cancel: &CancelToken,
) -> Result<bool> {
    let ha = History::from_segments(&session.location_segments(a.0, a.1, cancel).await?);
    let hb = History::from_segments(&session.location_segments(b.0, b.1, cancel).await?);
    Ok(youngest_common(&ha, &hb).is_some())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use svn_types::RepoDisplayPath;

    use super::*;

    fn seg(path: Option<&str>, start: u64, end: u64) -> LocationSegment {
        LocationSegment {
            range_start: Revnum(start),
            range_end: Revnum(end),
            path: path.map(|p| RepoDisplayPath::parse(p).expect("path")),
        }
    }

    fn common(a: &[LocationSegment], b: &[LocationSegment]) -> Option<(String, u64)> {
        youngest_common(&History::from_segments(a), &History::from_segments(b))
    }

    #[test]
    fn a_branch_meets_its_trunk_at_the_copy_source() {
        let trunk = [seg(Some("/trunk"), 1, 9)];
        let branch = [seg(Some("/branches/b"), 5, 9), seg(Some("/trunk"), 1, 3)];
        assert_eq!(common(&trunk, &branch), Some(("/trunk".into(), 3)));
    }

    #[test]
    fn two_branches_of_one_trunk_meet_at_the_younger_copy_source() {
        let b1 = [seg(Some("/b1"), 4, 9), seg(Some("/trunk"), 1, 2)];
        let b2 = [
            seg(Some("/b2"), 6, 9),
            seg(None, 6, 5),
            seg(Some("/trunk"), 1, 5),
        ];
        assert_eq!(common(&b1, &b2), Some(("/trunk".into(), 2)));
    }

    #[test]
    fn unrelated_paths_and_disjoint_revisions_share_nothing() {
        let docs = [seg(Some("/docs"), 1, 9)];
        let other = [seg(Some("/other"), 2, 9)];
        assert_eq!(common(&docs, &other), None);
        // The same path, deleted and added again: the two lives never overlap.
        let old = [seg(Some("/x"), 1, 3)];
        let new = [seg(Some("/x"), 5, 9)];
        assert_eq!(common(&old, &new), None);
        // A gap is not history.
        let gap = [seg(Some("/y"), 6, 9), seg(None, 1, 5)];
        assert_eq!(common(&gap, &[seg(None, 1, 5)]), None);
    }

    #[test]
    fn histories_that_meet_only_in_r0_share_the_root() {
        let a = [seg(Some("/"), 0, 0)];
        let b = [seg(Some("/x"), 1, 4), seg(Some("/"), 0, 0)];
        assert_eq!(common(&a, &b), Some(("/".into(), 0)));
        assert_eq!(common(&a, &[seg(Some("/x"), 1, 4)]), None);
    }
}

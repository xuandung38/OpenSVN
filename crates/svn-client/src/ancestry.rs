//! Common history of two repository locations, like `svn_client__get_youngest_common_ancestor`.
//!
//! The history of a location is the list of its location segments (every path the node had,
//! through every copy, and the revisions it had it in). Two locations are related when their
//! histories meet: the same path in a revision both cover. The youngest such meeting point is
//! their youngest common ancestor.

use svn_ra_api::{LocationSegment, RaSession};
use svn_types::{CancelToken, RelPath, Result, Revnum};

/// A node of the repository: its repository-relative path in a revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// Path below the repository root.
    pub relpath: RelPath,
    /// Revision.
    pub rev: Revnum,
}

/// The youngest common ancestor of `a` and `b`, `None` when they share no history.
///
/// Asks the server for both histories (one `get-location-segments` report each). A location
/// that does not exist fails with the server's error (`PathNotFound`).
pub async fn youngest_common_ancestor(
    session: &dyn RaSession,
    a: &Location,
    b: &Location,
    cancel: &CancelToken,
) -> Result<Option<Location>> {
    let history_a = session
        .get_location_segments(&a.relpath, Some(a.rev), None, None, cancel)
        .await?;
    let history_b = session
        .get_location_segments(&b.relpath, Some(b.rev), None, None, cancel)
        .await?;
    Ok(meeting_point(&history_a, &history_b))
}

/// The youngest location both histories hold. Gaps (segments without a path) never meet.
fn meeting_point(a: &[LocationSegment], b: &[LocationSegment]) -> Option<Location> {
    let mut best: Option<Location> = None;
    for sa in a {
        let Some(path) = &sa.path else { continue };
        for sb in b.iter().filter(|sb| sb.path.as_ref() == Some(path)) {
            let (lo, hi) = (
                sa.range_start.max(sb.range_start),
                sa.range_end.min(sb.range_end),
            );
            if lo <= hi && best.as_ref().is_none_or(|l| hi > l.rev) {
                best = Some(Location {
                    relpath: path.clone(),
                    rev: hi,
                });
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(path: Option<&str>, start: u64, end: u64) -> LocationSegment {
        LocationSegment {
            range_start: Revnum(start),
            range_end: Revnum(end),
            path: path.map(|p| RelPath::new(p).unwrap_or_else(|_| RelPath::root())),
        }
    }

    #[test]
    fn a_branch_meets_its_trunk_at_the_copy_source() {
        let trunk = [seg(Some("trunk"), 1, 9)];
        let branch = [
            seg(Some("branches/b"), 6, 8),
            seg(None, 5, 5),
            seg(Some("trunk"), 1, 4),
        ];
        assert_eq!(
            meeting_point(&trunk, &branch),
            Some(Location {
                relpath: RelPath::new("trunk").unwrap_or_else(|_| RelPath::root()),
                rev: Revnum(4),
            })
        );
        // Symmetric.
        assert_eq!(
            meeting_point(&branch, &trunk),
            meeting_point(&trunk, &branch)
        );
    }

    #[test]
    fn the_youngest_meeting_point_wins() {
        // Two branches of trunk: the later one was copied from the earlier one.
        let b1 = [seg(Some("branches/b1"), 5, 9), seg(Some("trunk"), 1, 4)];
        let b2 = [
            seg(Some("branches/b2"), 8, 9),
            seg(Some("branches/b1"), 5, 7),
            seg(Some("trunk"), 1, 4),
        ];
        assert_eq!(
            meeting_point(&b1, &b2).map(|l| (l.relpath.as_str().to_owned(), l.rev)),
            Some(("branches/b1".to_owned(), Revnum(7)))
        );
    }

    #[test]
    fn unrelated_nodes_and_gaps_never_meet() {
        let trunk = [seg(Some("trunk"), 1, 9)];
        let other = [seg(Some("other"), 2, 9)];
        assert_eq!(meeting_point(&trunk, &other), None);
        // The same path in revisions the other history does not cover: a node deleted and
        // added again is another node.
        let readded = [seg(Some("trunk"), 6, 9)];
        let old = [seg(Some("old"), 5, 9), seg(Some("trunk"), 1, 4)];
        assert_eq!(meeting_point(&readded, &old), None);
        assert_eq!(meeting_point(&[seg(None, 1, 9)], &[seg(None, 1, 9)]), None);
    }

    #[test]
    fn the_root_meets_itself_at_revision_zero() {
        let root = [seg(Some(""), 0, 3)];
        let copy_of_root = [seg(Some("branches/all"), 4, 4), seg(Some(""), 0, 0)];
        assert_eq!(
            meeting_point(&root, &copy_of_root),
            Some(Location {
                relpath: RelPath::root(),
                rev: Revnum(0),
            })
        );
    }
}

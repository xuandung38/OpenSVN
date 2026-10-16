//! Remote operation: node history. One `get-location-segments` REPORT.
//!
//! The answer is checked before anyone sees it: segments come youngest first, never overlap and
//! stay inside the revisions asked for. Nothing reaches the caller before the whole answer is
//! in, so a report that breaks off on a failed connection is asked for again from the start.

use svn_protocol::location_segments::{LocationSegmentsParser, location_segments_request};
use svn_ra_api::LocationSegment;
use svn_types::{CancelToken, ErrorCode, RelPath, Result, Revnum, SvnError};

use super::read::{corrupt, peg, stream_items};
use super::resume::ReadRetry;
use super::update::RaWire;

/// Location segments of the repository path `path`@`peg` in `end..=start` (see
/// [`svn_ra_api::RaSession::get_location_segments`]).
pub(crate) async fn location_segments(
    wire: &RaWire,
    path: &RelPath,
    peg_rev: Option<Revnum>,
    start: Option<Revnum>,
    end: Option<Revnum>,
    cancel: &CancelToken,
) -> Result<Vec<LocationSegment>> {
    let peg_rev = peg(wire, peg_rev, cancel).await?;
    let start = start.unwrap_or(peg_rev);
    let end = end.unwrap_or(Revnum(0));
    if start > peg_rev || end > start {
        return Err(SvnError::new(
            ErrorCode::Internal,
            format!(
                "invalid revision range {}:{} for the history of '{path}'@{peg_rev}",
                start.0, end.0
            ),
        ));
    }
    let mut retry = ReadRetry::new(wire, "node history");
    loop {
        let req = location_segments_request(wire.urls(), path, peg_rev, start, end)?;
        let attempt = async {
            let resp = wire.send_ok(req, cancel).await?;
            let mut out: Vec<LocationSegment> = Vec::new();
            let mut parser = LocationSegmentsParser::new();
            stream_items(resp, &mut parser, cancel, |s| {
                let below = out.last().map_or(start.0 + 1, |prev| prev.range_start.0);
                if s.range_end.0 >= below || s.range_start < end {
                    return Err(corrupt(format!(
                        "location segment {}:{} is out of order or outside {}:{}",
                        s.range_start.0, s.range_end.0, start.0, end.0
                    )));
                }
                out.push(LocationSegment {
                    range_start: s.range_start,
                    range_end: s.range_end,
                    path: s.path,
                });
                Ok(())
            })
            .await?;
            Ok(out)
        }
        .await;
        match attempt {
            Ok(segments) => return Ok(segments),
            Err(e) => retry.again(e, cancel).await?,
        }
    }
}

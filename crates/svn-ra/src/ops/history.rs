//! Remote operation: location history (`get-location-segments`), for the ancestry check of a
//! switch.

use svn_protocol::location_segments::{LocationSegmentsParser, location_segments_request};
use svn_ra_api::LocationSegment;
use svn_types::{CancelToken, RelPath, Result, Revnum};

use super::read::stream_items;
use super::resume::ReadRetry;
use super::update::RaWire;

/// Location history of `repos_path` (repository-relative) at `peg`, youngest segment first. The
/// answer is collected whole before it is returned, so a report that breaks off is simply asked
/// again.
pub(crate) async fn location_segments(
    wire: &RaWire,
    repos_path: &RelPath,
    peg: Revnum,
    cancel: &CancelToken,
) -> Result<Vec<LocationSegment>> {
    let mut retry = ReadRetry::new(wire, "location history");
    loop {
        let req = location_segments_request(wire.urls(), repos_path, peg)?;
        let attempt = async {
            let resp = wire.send_ok(req, cancel).await?;
            let mut parser = LocationSegmentsParser::new();
            let mut out = Vec::new();
            stream_items(resp, &mut parser, cancel, |s| {
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
            Ok(out) => return Ok(out),
            Err(e) => retry.again(e, cancel).await?,
        }
    }
}

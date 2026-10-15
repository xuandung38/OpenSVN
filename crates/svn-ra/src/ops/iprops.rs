//! Remote operation: inherited properties. One `inherited-props-report` REPORT on the
//! peg-revision URL of the node (svn 1.8+ servers advertise `inherited-props`).

use svn_protocol::iprops::{InheritedPropsParser, inherited_props_request};
use svn_ra_api::InheritedItem;
use svn_types::{CancelToken, ErrorCode, RelPath, Result, Revnum, SvnError};

use super::read::stream_items;
use super::resume::ReadRetry;
use super::update::RaWire;

/// Properties `repos_relpath` (repository-relative) inherits in `rev`, the root first.
pub(crate) async fn inherited_props(
    wire: &RaWire,
    repos_relpath: &RelPath,
    rev: Revnum,
    cancel: &CancelToken,
) -> Result<Vec<InheritedItem>> {
    if !wire.caps().dav.inherited_props {
        // libsvn_ra_serf walks the parents with PROPFIND here; servers older than 1.8 are
        // not a target, so the cache is left as it is instead.
        return Err(SvnError::new(
            ErrorCode::ServerUnsupported,
            "the server cannot report inherited properties",
        ));
    }
    // Nothing reaches the caller before the whole answer is in: a report that breaks off is
    // asked for again from the start.
    let mut retry = ReadRetry::new(wire, "inherited properties");
    loop {
        let req = inherited_props_request(wire.urls(), rev, repos_relpath)?;
        let attempt = async {
            let resp = wire.send_ok(req, cancel).await?;
            let mut items = Vec::new();
            let mut parser = InheritedPropsParser::new();
            stream_items(resp, &mut parser, cancel, |item| {
                items.push(InheritedItem {
                    repos_relpath: item.path,
                    props: item.props,
                });
                Ok(())
            })
            .await?;
            Ok(items)
        }
        .await;
        match attempt {
            Ok(items) => return Ok(items),
            Err(e) => retry.again(e, cancel).await?,
        }
    }
}

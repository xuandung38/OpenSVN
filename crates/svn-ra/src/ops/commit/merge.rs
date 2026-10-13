//! Transaction lifecycle requests: `MERGE`, abort, and `PROPPATCH` answer checking.

use svn_protocol::commit::{
    LockHandling, MergeOptions, MergeResponse, abort_txn_request, parse_merge_response,
};
use svn_protocol::{ResponseHead, TxnName, map_http_status};
use svn_types::{CancelToken, ErrorCode, RelPath, Result, SvnError};

use crate::ops::update::RaWire;

/// Merges the transaction into a new revision.
pub(super) async fn merge(
    wire: &RaWire,
    txn: &TxnName,
    keep_locks: bool,
    lock_tokens: &[(RelPath, String)],
    cancel: &CancelToken,
) -> Result<MergeResponse> {
    let opts = MergeOptions {
        locks: if keep_locks {
            LockHandling::Keep
        } else {
            LockHandling::Release
        },
        no_merge_response: true,
        lock_tokens,
    };
    let req = svn_protocol::commit::merge_request(wire.urls(), wire.session_rel(), txn, &opts)?;
    let resp = wire.send(req, cancel).await?;
    let head = ResponseHead::new(
        resp.status().as_u16(),
        crate::ops::update::headers_of(&resp),
    );
    let body = RaWire::read_small(resp).await?;
    parse_merge_response(&head, &body)
}

/// Deletes a transaction; a transaction that is already gone counts as deleted.
pub(super) async fn delete_txn(wire: &RaWire, txn: &TxnName, cancel: &CancelToken) -> Result<()> {
    delete_txn_found(wire, txn, cancel).await.map(|_| ())
}

/// Like [`delete_txn`], telling whether the transaction existed (`true`) or was already gone.
/// HTTP 400 counts as "not found": a server answers it for a name that is not a transaction of
/// the requested kind.
pub(super) async fn delete_txn_found(
    wire: &RaWire,
    txn: &TxnName,
    cancel: &CancelToken,
) -> Result<bool> {
    let req = abort_txn_request(wire.urls(), txn)?;
    let resp = wire.send(req, cancel).await?;
    let status = resp.status().as_u16();
    if resp.status().is_success() {
        return Ok(true);
    }
    if matches!(status, 400 | 404) {
        return Ok(false);
    }
    Err(crate::ops::update::error_of(resp, cancel).await)
}

/// A `PROPPATCH` answers `207` even when a property was refused; the verdict is per property.
pub(super) fn check_proppatch(body: &[u8]) -> Result<()> {
    let text = String::from_utf8_lossy(body);
    let mut rest: &str = &text;
    let mut first_failure: Option<u16> = None;
    while let Some(at) = rest.find("HTTP/1.") {
        rest = &rest[at + "HTTP/1.".len()..];
        // "1 207 ..." or "0 200 ...": skip the minor version digit and the space.
        let status: Option<u16> = rest.get(2..5).and_then(|s| s.parse().ok());
        match status {
            Some(s) if (200..300).contains(&s) => {}
            // 424 only says "not done because another property failed".
            Some(424) => {}
            Some(s) => {
                first_failure.get_or_insert(s);
            }
            None => {
                return Err(SvnError::new(
                    ErrorCode::Corrupt,
                    "unreadable status in PROPPATCH answer",
                ));
            }
        }
    }
    match first_failure {
        None => Ok(()),
        Some(s) => Err(SvnError::new(
            map_http_status(s),
            format!("server refused a property change (HTTP {s})"),
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn proppatch_success_and_failure() {
        let ok = b"<D:multistatus><D:propstat><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:multistatus>";
        assert!(check_proppatch(ok).is_ok());
        let bad = b"<D:status>HTTP/1.1 403 Forbidden</D:status><D:status>HTTP/1.1 424 Failed Dependency</D:status>";
        assert_eq!(check_proppatch(bad).unwrap_err().code, ErrorCode::Forbidden);
        assert!(check_proppatch(b"").is_ok());
    }
}

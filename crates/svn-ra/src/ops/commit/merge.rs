//! Transaction lifecycle requests: `MERGE`, abort, and `PROPPATCH` answer checking.

use svn_protocol::commit::{
    LockHandling, MergeOptions, MergeResponse, abort_txn_request, parse_merge_response,
};
use svn_protocol::xml::{Ns, XmlEvent, read_all};
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
    let body = RaWire::read_small(resp, cancel).await?;
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
///
/// Parses the multistatus and looks only at `DAV:status` elements (of `DAV:propstat` or
/// `DAV:response`), so text elsewhere, such as a `DAV:responsedescription`, cannot fake a status.
pub(super) fn check_proppatch(body: &[u8]) -> Result<()> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    let mut status_text: Option<String> = None;
    let mut first_failure: Option<u16> = None;
    for event in read_all(body)? {
        match event {
            XmlEvent::Start(tag) if tag.name.is(&Ns::Dav, "status") => {
                status_text = Some(String::new());
            }
            XmlEvent::Text(t) => {
                if let Some(buf) = status_text.as_mut() {
                    buf.push_str(&t);
                }
            }
            XmlEvent::End(name) if name.is(&Ns::Dav, "status") => {
                let line = status_text.take().unwrap_or_default();
                match status_code(&line)? {
                    s if (200..300).contains(&s) => {}
                    // 424 only says "not done because another property failed".
                    424 => {}
                    s => {
                        first_failure.get_or_insert(s);
                    }
                }
            }
            _ => {}
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

/// Code of an `HTTP/1.1 207 Multi-Status` style status line.
fn status_code(line: &str) -> Result<u16> {
    let mut parts = line.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(v), Some(code)) if v.starts_with("HTTP/") && code.len() == 3 => {
            code.parse().map_err(|_| unreadable_status())
        }
        _ => Err(unreadable_status()),
    }
}

fn unreadable_status() -> SvnError {
    SvnError::new(ErrorCode::Corrupt, "unreadable status in PROPPATCH answer")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn proppatch_success_and_failure() {
        let ok = br#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/x</D:href><D:propstat><D:prop/><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
        assert!(check_proppatch(ok).is_ok());
        let bad = br#"<D:multistatus xmlns:D="DAV:"><D:response><D:propstat><D:status>HTTP/1.1 403 Forbidden</D:status></D:propstat><D:propstat><D:status>HTTP/1.1 424 Failed Dependency</D:status></D:propstat></D:response></D:multistatus>"#;
        assert_eq!(check_proppatch(bad).unwrap_err().code, ErrorCode::Forbidden);
        assert!(check_proppatch(b"").is_ok());
    }

    #[test]
    fn proppatch_ignores_status_text_outside_status_elements() {
        let body = br#"<D:multistatus xmlns:D="DAV:"><D:response><D:propstat><D:status>HTTP/1.1 200 OK</D:status></D:propstat><D:responsedescription>was HTTP/1.1 500 before</D:responsedescription></D:response></D:multistatus>"#;
        assert!(check_proppatch(body).is_ok());
        // A failure stated only in a description is not a status either way.
        let body = br#"<D:multistatus xmlns:D="DAV:"><D:response><D:status>HTTP/1.1 403 Forbidden</D:status><D:responsedescription>HTTP/1.1 200 OK</D:responsedescription></D:response></D:multistatus>"#;
        assert_eq!(
            check_proppatch(body).unwrap_err().code,
            ErrorCode::Forbidden
        );
    }

    #[test]
    fn proppatch_rejects_garbage() {
        let body =
            br#"<D:multistatus xmlns:D="DAV:"><D:status>nonsense</D:status></D:multistatus>"#;
        assert_eq!(check_proppatch(body).unwrap_err().code, ErrorCode::Corrupt);
        assert_eq!(
            check_proppatch(b"<unclosed").unwrap_err().code,
            ErrorCode::Corrupt
        );
    }
}

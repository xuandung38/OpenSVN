//! Transaction lifecycle requests: `MERGE`, abort, and `PROPPATCH` answer checking.

use std::time::Duration;

use svn_protocol::commit::{
    LockHandling, MergeOptions, MergeResponse, abort_txn_request, parse_merge_response,
};
use svn_protocol::errors::parse_dav_error;
use svn_protocol::xml::{Ns, XmlEvent, read_all};
use svn_protocol::{ParserLimits, ResponseHead, TxnName, error_from_response, map_http_status};
use svn_types::{CancelToken, ErrorCode, RelPath, Result, SvnError};

use crate::ops::update::RaWire;

/// How long a best-effort cleanup `DELETE` of a transaction may take before it is abandoned.
pub(super) const CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);

/// Why a commit ended without a new revision, from the point of view of the transaction.
#[derive(Debug)]
pub(super) enum CommitFailure {
    /// The failure came before the `MERGE` was sent, or the server refused the `MERGE` (see
    /// [`refusal_is_definite`]). The transaction is deleted, as libsvn_ra_serf does.
    NotMerged(SvnError),
    /// The `MERGE` was sent but its outcome is unknown; the transaction must be kept. The error
    /// has code [`ErrorCode::CommitOutcomeUnknown`].
    OutcomeUnknown(SvnError),
}

impl From<SvnError> for CommitFailure {
    fn from(e: SvnError) -> Self {
        Self::NotMerged(e)
    }
}

/// Merges the transaction into a new revision.
///
/// Mirrors libsvn_ra_serf: once the request may have reached the server, only a refusal (see
/// [`refusal_is_definite`]) is reported as a failed commit. Cancellation, a
/// transport failure, a gateway timeout or bare 5xx, and an unreadable 2xx answer all yield
/// [`CommitFailure::OutcomeUnknown`], whose child is the underlying error.
pub(super) async fn merge(
    wire: &RaWire,
    txn: &TxnName,
    keep_locks: bool,
    lock_tokens: &[(RelPath, String)],
    cancel: &CancelToken,
) -> std::result::Result<MergeResponse, CommitFailure> {
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
    // Nothing has been sent yet: a cancellation here is a plain cancellation.
    cancel.check()?;
    let unknown = |e: SvnError| CommitFailure::OutcomeUnknown(outcome_unknown(txn, e));
    let resp = wire.send(req, cancel).await.map_err(unknown)?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let headers = crate::ops::update::headers_of(&resp);
        // A body that cannot be read must not hide the status; it only makes the answer
        // less definite.
        let body = crate::ops::update::read_limited(
            resp,
            ParserLimits::default().max_error_body_bytes,
            cancel,
        )
        .await
        .unwrap_or_default();
        let err = error_from_response(status, &headers, &body);
        let has_dav_error = parse_dav_error(&body).is_some();
        return Err(if refusal_is_definite(status, has_dav_error) {
            // Out of date, hook failure, lock error, authz refusal...
            CommitFailure::NotMerged(err)
        } else {
            unknown(err)
        });
    }
    let head = ResponseHead::new(
        resp.status().as_u16(),
        crate::ops::update::headers_of(&resp),
    );
    let body = RaWire::read_small(resp, cancel).await.map_err(unknown)?;
    parse_merge_response(&head, &body).map_err(unknown)
}

/// Whether a non-2xx answer to a `MERGE` is treated as the server refusing the commit (the
/// transaction is then deleted and the error returned as an ordinary failure).
///
/// This is how libsvn_ra_serf reads the answer, and it is right for every ordinary failure, but
/// it is not a proof that nothing was merged: mod_dav_svn can answer a 500 with a `D:error`,
/// or a 404 (transaction already gone), after the revision was written, when the repository
/// storage fails during the post-commit steps (for example purging the transaction or
/// updating `rep-cache`). Callers that journal the `MERGE`
/// ([`svn_ra_api::CommitObserver::on_merge_sending`]) reconcile with the log before they drop
/// the journal.
///
/// - `408`, `502` and `504` never count: a reverse proxy answers them on a timeout or a broken
///   upstream while the backend may still be running the commit (a slow pre-commit hook).
/// - A 5xx answer counts only with a `D:error` body: that is mod_dav_svn reporting a failed
///   commit (for example a hook failure). A bare 5xx may come from a proxy or a crashed
///   worker after the commit started.
/// - A 4xx answer counts with a `D:error` body as well (out of date, lock errors), and also
///   without one: HTTP 4xx normally means the request was refused before it was carried out
///   (authz, authentication, proxy limits).
fn refusal_is_definite(status: u16, has_dav_error: bool) -> bool {
    match status {
        408 | 502 | 504 => false,
        400..=499 => true,
        500..=599 => has_dav_error,
        // 1xx/3xx: not a refusal anyone can interpret here.
        _ => false,
    }
}

/// The error returned when a `MERGE` was sent but its result is unknown.
fn outcome_unknown(txn: &TxnName, cause: SvnError) -> SvnError {
    SvnError::new(
        ErrorCode::CommitOutcomeUnknown,
        format!(
            "the commit may or may not have been applied: the result of merging transaction \
             '{}' is unknown, so the transaction was kept; check the repository's youngest \
             revision before retrying",
            txn.name
        ),
    )
    .with_child(cause)
}

/// Deletes a transaction; a transaction that is already gone counts as deleted.
pub(super) async fn delete_txn(wire: &RaWire, txn: &TxnName, cancel: &CancelToken) -> Result<()> {
    delete_txn_found(wire, txn, cancel, false).await.map(|_| ())
}

/// Best-effort cleanup delete of a transaction: never prompts for credentials or certificates
/// (only cached credentials are used) and gives up after [`CLEANUP_TIMEOUT`]. Safe to run from
/// a detached task after the user has moved on. Says whether the transaction still existed
/// (and is now deleted).
pub(super) async fn cleanup_txn(wire: &RaWire, txn: &TxnName) -> Result<bool> {
    // Cleanup must work after the operation was cancelled, so it has its own token.
    let cancel = CancelToken::new();
    let work = delete_txn_found(wire, txn, &cancel, true);
    match tokio::time::timeout(CLEANUP_TIMEOUT, work).await {
        Ok(r) => r,
        Err(_) => {
            cancel.cancel();
            Err(SvnError::new(
                ErrorCode::Timeout,
                "deleting the commit transaction timed out",
            ))
        }
    }
}

/// How the server answered a `DELETE` of a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeleteAnswer {
    /// 2xx: the transaction existed and is deleted.
    Deleted,
    /// 404: no transaction of that name.
    NotFound,
    /// 400: the name is not a transaction of the requested kind (plain or virtual).
    WrongKind,
    /// 5xx: the server failed while answering (the transaction may or may not still exist);
    /// the server's error message.
    ServerError(String),
}

/// Sends a `DELETE` of `txn` and classifies the answer; any other status is an error.
pub(super) async fn delete_txn_answer(
    wire: &RaWire,
    txn: &TxnName,
    cancel: &CancelToken,
) -> Result<DeleteAnswer> {
    let req = abort_txn_request(wire.urls(), txn)?;
    let resp = wire.send(req, cancel).await?;
    if resp.status().is_success() {
        return Ok(DeleteAnswer::Deleted);
    }
    match resp.status().as_u16() {
        404 => Ok(DeleteAnswer::NotFound),
        400 => Ok(DeleteAnswer::WrongKind),
        500..=599 => Ok(DeleteAnswer::ServerError(
            crate::ops::update::error_of(resp, cancel).await.message,
        )),
        _ => Err(crate::ops::update::error_of(resp, cancel).await),
    }
}

/// Like [`delete_txn`], telling whether the transaction existed (`true`) or was already gone.
/// HTTP 400 counts as "not found": a server answers it for a name that is not a transaction of
/// the requested kind. `unattended` sends without ever prompting.
pub(super) async fn delete_txn_found(
    wire: &RaWire,
    txn: &TxnName,
    cancel: &CancelToken,
    unattended: bool,
) -> Result<bool> {
    let req = abort_txn_request(wire.urls(), txn)?;
    let resp = if unattended {
        wire.send_unattended(req, cancel).await?
    } else {
        wire.send(req, cancel).await?
    };
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
    SvnError::new(
        ErrorCode::ProtocolViolation,
        "unreadable status in PROPPATCH answer",
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn which_merge_refusals_are_definite() {
        for status in [408, 502, 504] {
            assert!(!refusal_is_definite(status, true), "{status}");
            assert!(!refusal_is_definite(status, false), "{status}");
        }
        for status in [400, 401, 403, 404, 409, 412, 413, 423] {
            assert!(refusal_is_definite(status, true), "{status}");
            assert!(refusal_is_definite(status, false), "{status}");
        }
        for status in [500, 501, 503, 507] {
            assert!(refusal_is_definite(status, true), "{status}");
            assert!(!refusal_is_definite(status, false), "{status}");
        }
        assert!(!refusal_is_definite(304, true));
    }

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
        assert_eq!(
            check_proppatch(body).unwrap_err().code,
            ErrorCode::ProtocolViolation
        );
        assert_eq!(
            check_proppatch(b"<unclosed").unwrap_err().code,
            ErrorCode::ProtocolViolation
        );
    }
}

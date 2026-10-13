//! Commit over HTTPv2: transaction creation, the [`HttpCommitEditor`] and `abort_txn`.

mod editor;
mod merge;
mod put;

use std::sync::Arc;

use svn_protocol::TxnName;
use svn_protocol::commit::{
    create_txn_plan, ephemeral_txn_props, parse_create_txn_response, txn_proppatch_request,
};
use svn_ra_api::{CommitEditor, CommitObserver};
use svn_types::{CancelToken, ErrorCode, PropChange, RelPath, Result};

pub use editor::HttpCommitEditor;

use crate::ops::update::{RaWire, headers_of};

/// Client compatibility level announced to the server with the transaction.
const COMPAT_VERSION: &str = "1.15.0";

/// Starts a commit: creates the transaction (with `message` and `rev_props` as revision
/// properties), tells `observer` about it before any content is sent, and returns the editor.
///
/// `lock_tokens` are `(path relative to the session URL, token)` pairs. If the observer
/// rejects the transaction it is deleted and the observer's error is returned.
///
/// The returned editor must end with `close_edit` or `abort_edit`. Dropping it without either
/// leaves the transaction on the server unless the best-effort background delete described on
/// [`HttpCommitEditor`] gets through; the journaled name and [`abort_txn`] are the reliable way
/// to clean up.
///
/// `close_edit` fails with [`ErrorCode::CommitOutcomeUnknown`] when the final `MERGE` was sent
/// but its result is unknown; the transaction is then kept for the caller to reconcile (see
/// [`HttpCommitEditor`]).
pub async fn start_commit(
    wire: &RaWire,
    message: &str,
    rev_props: &[PropChange],
    lock_tokens: &[(String, String)],
    keep_locks: bool,
    observer: Arc<dyn CommitObserver>,
    cancel: &CancelToken,
) -> Result<Box<dyn CommitEditor>> {
    cancel.check()?;
    let locks = lock_tokens
        .iter()
        .map(|(p, t)| Ok((RelPath::new(p)?, t.clone())))
        .collect::<Result<Vec<_>>>()?;

    let mut props: Vec<(String, Vec<u8>)> =
        vec![("svn:log".to_owned(), message.as_bytes().to_vec())];
    for p in rev_props {
        if let Some(v) = &p.value
            && !props.iter().any(|(n, _)| n == &p.name)
        {
            props.push((p.name.clone(), v.to_vec()));
        }
    }
    for (n, v) in ephemeral_txn_props(
        wire.caps(),
        COMPAT_VERSION,
        concat!("OpenSVN/", env!("CARGO_PKG_VERSION")),
    ) {
        props.push((n.to_owned(), v));
    }
    let refs: Vec<(&str, &[u8])> = props
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_slice()))
        .collect();
    let plan = create_txn_plan(wire.urls(), wire.caps(), &refs)?;

    let resp = wire.send(plan.request, cancel).await?;
    let head = svn_protocol::ResponseHead::new(resp.status().as_u16(), headers_of(&resp));
    let body = RaWire::read_small(resp, cancel).await?;
    let txn = parse_create_txn_response(&head, &body)?;

    if let Err(e) = begin(wire, &txn, &plan.deferred_revprops, &*observer, cancel).await {
        // Best effort: the transaction must not outlive a failed start.
        let _ = merge::cleanup_txn(wire, &txn).await;
        return Err(e);
    }
    Ok(Box::new(HttpCommitEditor::new(
        wire.clone(),
        txn,
        cancel.clone(),
        observer,
        locks,
        keep_locks,
    )))
}

async fn begin(
    wire: &RaWire,
    txn: &TxnName,
    deferred: &[PropChange],
    observer: &dyn CommitObserver,
    cancel: &CancelToken,
) -> Result<()> {
    // Journal first: a crash from here on can be cleaned up by name.
    observer.on_txn_created(&txn.name)?;
    if !deferred.is_empty() {
        let req = txn_proppatch_request(wire.urls(), txn, deferred)?;
        let resp = wire.send_ok(req, cancel).await?;
        merge::check_proppatch(&RaWire::read_small(resp, cancel).await?)?;
    }
    Ok(())
}

/// Deletes a leaked server transaction by name. Idempotent: a missing transaction is success.
pub async fn abort_txn(wire: &RaWire, txn_name: &str) -> Result<()> {
    let cancel = CancelToken::new();
    // A journaled name does not say whether the transaction was plain or virtual.
    match merge::delete_txn_found(wire, &TxnName::plain(txn_name), &cancel, false).await {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        // No plain stub announced: only virtual transactions exist on this server.
        Err(e) if e.code == ErrorCode::ServerUnsupported => {}
        Err(e) => return Err(e),
    }
    if wire.caps().vtxn_stub.is_none() {
        return Ok(());
    }
    merge::delete_txn(wire, &TxnName::virtual_txn(txn_name), &cancel).await
}

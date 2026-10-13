//! One remote check of a working copy: ask the engine what changed on the server, record the
//! incoming count and announce it. Shared by the `status_remote` command and the periodic poller.

use std::sync::Arc;

use opensvn_ipc::AppError;

use super::publisher::StatusPublisher;
use crate::engine::{EngineFacade, OpContext};
use crate::wc::{ResolvedWc, WcRegistry};

/// Runs the check through the working copy's actor (a read: no lock is taken).
pub async fn run_remote_check(
    engine: &Arc<dyn EngineFacade>,
    publisher: &StatusPublisher,
    wcs: &WcRegistry,
    wc: &ResolvedWc,
    ctx: &OpContext,
) -> Result<(), AppError> {
    let scan = wc
        .actor
        .read(|| {
            let (engine, wc_ref, ctx) = (Arc::clone(engine), wc.wc.clone(), ctx.clone());
            async move {
                engine
                    .status_remote(&wc_ref, &ctx)
                    .await
                    .map_err(|e| AppError::from(&e))
            }
        })
        .await?;
    wcs.set_incoming(&wc.wc.id, scan.incoming);
    publisher.apply_remote(&wc.wc.id, &scan).await
}

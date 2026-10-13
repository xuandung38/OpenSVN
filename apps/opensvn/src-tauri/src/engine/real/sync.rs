//! Operations that bring a working copy to a repository state through `svn-client`: checkout,
//! update, switch and depth changes.
//!
//! Not wired yet: every method fails with the `not wired` error until the sync path lands.

use opensvn_ipc::{Depth, Rev, UpdateRequest};

use super::{RealEngine, not_wired};
use crate::engine::context::OpContext;
use crate::engine::facade::{CheckoutPlan, EngineResult, WcInfo, WcRef};

impl RealEngine {
    pub(super) async fn do_checkout(
        &self,
        _plan: &CheckoutPlan,
        ctx: &OpContext,
    ) -> EngineResult<WcInfo> {
        // Cancellation wins over everything else, as for every wired operation.
        ctx.check()?;
        Err(not_wired("checkout"))
    }

    pub(super) async fn do_update(
        &self,
        _wc: &WcRef,
        _req: &UpdateRequest,
        _ctx: &OpContext,
    ) -> EngineResult<Rev> {
        Err(not_wired("update"))
    }

    pub(super) async fn do_switch(
        &self,
        _wc: &WcRef,
        _url: &str,
        _rev: Option<Rev>,
        _ctx: &OpContext,
    ) -> EngineResult<Rev> {
        Err(not_wired("switch"))
    }

    pub(super) async fn do_set_depth(
        &self,
        _wc: &WcRef,
        _path: &str,
        _depth: Depth,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("set_depth"))
    }
}

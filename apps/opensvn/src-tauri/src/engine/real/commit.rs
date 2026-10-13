//! Operations that send working copy changes to the repository through `svn-client`: commit and
//! copying a working copy path to a URL.
//!
//! Not wired yet: every method fails with the `not wired` error until the commit path lands.

use opensvn_ipc::{CommitRequest, CopyLocationRequest, Rev};

use super::{RealEngine, not_wired};
use crate::engine::context::OpContext;
use crate::engine::facade::{EngineResult, WcRef};

impl RealEngine {
    pub(super) async fn do_commit(
        &self,
        _wc: &WcRef,
        _req: &CommitRequest,
        _ctx: &OpContext,
    ) -> EngineResult<Rev> {
        Err(not_wired("commit"))
    }

    pub(super) async fn do_copy_location_from_wc(
        &self,
        _req: &CopyLocationRequest,
        _wc: &WcRef,
        ctx: &OpContext,
    ) -> EngineResult<Rev> {
        ctx.check()?;
        Err(not_wired("copy_location from a working copy"))
    }
}

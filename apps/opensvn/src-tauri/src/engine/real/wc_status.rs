//! Working copy reads through `svn-client`: status, working copy diff and remote status.
//!
//! Not wired yet: every method fails with the `not wired` error until the read path lands.

use opensvn_ipc::{FileDiff, FileStatus, Rev};

use super::{RealEngine, not_wired};
use crate::engine::context::OpContext;
use crate::engine::facade::{DiffOptions, EngineResult, RemoteScan, StatusQuery, WcRef};

impl RealEngine {
    pub(super) async fn do_status(
        &self,
        _wc: &WcRef,
        _query: StatusQuery,
        _ctx: &OpContext,
    ) -> EngineResult<Vec<FileStatus>> {
        Err(not_wired("status"))
    }

    pub(super) async fn do_diff_wc(
        &self,
        _wc: &WcRef,
        _path: &str,
        _against: Option<Rev>,
        _opts: DiffOptions,
        _ctx: &OpContext,
    ) -> EngineResult<FileDiff> {
        Err(not_wired("diff_wc"))
    }

    pub(super) async fn do_status_remote(
        &self,
        _wc: &WcRef,
        _ctx: &OpContext,
    ) -> EngineResult<RemoteScan> {
        Err(not_wired("status_remote"))
    }
}

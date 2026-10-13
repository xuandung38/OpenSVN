//! Local working copy edits through `svn-client`: revert, add, delete, move, resolve, cleanup,
//! changelists, ignore patterns, properties, move marking and format upgrades.
//!
//! Not wired yet: every method fails with the `not wired` error until the local path lands.

use opensvn_ipc::{CleanupRequest, IgnoreAddRequest, ResolveRequest, RevertRequest};

use super::{RealEngine, not_wired};
use crate::engine::context::OpContext;
use crate::engine::facade::{EngineResult, WcRef};

impl RealEngine {
    pub(super) async fn do_changelist_set(
        &self,
        _wc: &WcRef,
        _paths: &[String],
        _name: Option<&str>,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("changelist_set"))
    }

    pub(super) async fn do_ignore_add(
        &self,
        _wc: &WcRef,
        _req: &IgnoreAddRequest,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("ignore_add"))
    }

    pub(super) async fn do_mark_move(
        &self,
        _wc: &WcRef,
        _from: &str,
        _to: &str,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("mark_move"))
    }

    pub(super) async fn do_resolve(
        &self,
        _wc: &WcRef,
        _req: &ResolveRequest,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("resolve"))
    }

    pub(super) async fn do_props_set(
        &self,
        _wc: &WcRef,
        _path: &str,
        _name: &str,
        _value: Option<&str>,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("props_set"))
    }

    pub(super) async fn do_revert(
        &self,
        _wc: &WcRef,
        _req: &RevertRequest,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("revert"))
    }

    pub(super) async fn do_add_paths(
        &self,
        _wc: &WcRef,
        _paths: &[String],
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("add_paths"))
    }

    pub(super) async fn do_delete_paths(
        &self,
        _wc: &WcRef,
        _paths: &[String],
        _keep_local: bool,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("delete_paths"))
    }

    pub(super) async fn do_move_path(
        &self,
        _wc: &WcRef,
        _from: &str,
        _to: &str,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("move_path"))
    }

    pub(super) async fn do_cleanup(
        &self,
        _wc: &WcRef,
        _req: &CleanupRequest,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("cleanup"))
    }

    pub(super) async fn do_upgrade_format(
        &self,
        _wc: &WcRef,
        _to: u8,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("upgrade_format"))
    }
}

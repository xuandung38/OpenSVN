//! Local working copy edits through `svn-client`: revert, add, delete, move, resolve, cleanup,
//! changelists, ignore patterns and properties.
//!
//! Every call opens the registered working copy root on a blocking thread
//! (`svn_client::wc::open_containing`, lock owner from [`RealEngine::client_ctx`]) and lets
//! `svn-client` take the working copy write lock; the app serializes calls per working copy
//! (`WcActor`), so no app-level lock is added. Request paths are validated with
//! [`local_util::target`] before anything is touched.
//!
//! Marking a move made outside the app and changing the working copy format are not supported
//! by `svn-client` yet and fail with a user-facing error.

use opensvn_ipc::{
    CleanupRequest, IgnoreAddRequest, IgnoreScope, ResolveChoice, ResolveRequest, RevertRequest,
};
use svn_client::local::{
    self as client_local, AddOptions, DeleteOptions, PropSetOptions, ResolveChoice as Accept,
    ResolveScope, RevertOptions,
};
use svn_types::{Depth, ErrorCode, RelPath, SvnError};
use svn_wc::WcDb;

use super::RealEngine;
use super::local_util::{self, LocalEnv};
use super::wc_read::blocking;
use crate::engine::context::OpContext;
use crate::engine::facade::{EngineResult, WcRef};

/// Issue tracking `svn-client` support for the operations this engine refuses.
const UNSUPPORTED_ISSUE: &str = "https://github.com/xuandung38/OpenSVN/issues/90";

/// `ResolveChoice` of the app -> `--accept` choice of `svn-client`.
pub(super) fn accept_of(choice: ResolveChoice) -> Accept {
    match choice {
        ResolveChoice::Mine => Accept::MineFull,
        ResolveChoice::Theirs => Accept::TheirsFull,
        ResolveChoice::Working => Accept::Working,
        ResolveChoice::Base => Accept::Base,
    }
}

/// Changelist name check shared with the fake engine.
fn check_changelist_name(name: Option<&str>) -> EngineResult<()> {
    if let Some(n) = name
        && (n.trim().is_empty() || n.len() > 100 || n.chars().any(char::is_control))
    {
        return Err(local_util::invalid(format!(
            "invalid changelist name {n:?}"
        )));
    }
    Ok(())
}

impl RealEngine {
    /// Runs `f` on a blocking thread with the working copy at `wc.root` opened for writing.
    async fn with_local<T: Send + 'static>(
        &self,
        wc: &WcRef,
        ctx: &OpContext,
        f: impl FnOnce(&WcDb, &LocalEnv) -> EngineResult<T> + Send + 'static,
    ) -> EngineResult<T> {
        ctx.check()?;
        let client = self.client_ctx(ctx);
        let config = self.svn_config.clone();
        let (root, cancel) = (wc.root.clone(), ctx.cancel.clone());
        blocking(move || {
            let db = local_util::open_wc_root(&client, &root)?;
            let env = local_util::local_env(&config, &db, cancel)?;
            f(&db, &env)
        })
        .await
    }

    pub(super) async fn do_changelist_set(
        &self,
        wc: &WcRef,
        paths: &[String],
        name: Option<&str>,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        check_changelist_name(name)?;
        let (paths, name) = (paths.to_vec(), name.map(str::to_owned));
        self.with_local(wc, ctx, move |db, env| {
            let targets = local_util::targets(db, &paths)?;
            client_local::set_changelist(db, &env.cancel, &targets, name.as_deref(), Depth::Empty)
                .map(|_| ())
        })
        .await
    }

    pub(super) async fn do_ignore_add(
        &self,
        wc: &WcRef,
        req: &IgnoreAddRequest,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        let pattern = local_util::checked_ignore_pattern(&req.pattern)?.to_owned();
        let (dir, scope) = (req.dir.clone(), req.scope);
        self.with_local(wc, ctx, move |db, env| {
            let (rel, _) = local_util::target(db, &dir)?;
            if db.read_node(&rel)?.is_none() {
                return Err(SvnError::new(
                    ErrorCode::PathNotFound,
                    format!("'{rel}' is not under version control"),
                ));
            }
            match scope {
                IgnoreScope::SvnIgnore => {
                    client_local::ignore_add(db, &env.cancel, &rel, &[pattern])
                }
                // Kept in the administrative directory of this working copy only, like the fake
                // engine's per-working-copy list; status and add treat it as a global ignore.
                IgnoreScope::Local => local_util::add_local_ignore(db.wcroot(), &pattern),
            }
        })
        .await
    }

    pub(super) async fn do_mark_move(
        &self,
        _wc: &WcRef,
        _from: &str,
        _to: &str,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        ctx.check()?;
        Err(SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!(
                "Marking a move made outside OpenSVN is not supported yet. Revert both paths and \
                 move the item with OpenSVN's Move command instead (tracked in {UNSUPPORTED_ISSUE})."
            ),
        ))
    }

    pub(super) async fn do_resolve(
        &self,
        wc: &WcRef,
        req: &ResolveRequest,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        let (path, accept) = (req.path.clone(), accept_of(req.choice));
        self.with_local(wc, ctx, move |db, env| {
            let (rel, _) = local_util::target(db, &path)?;
            client_local::resolve(db, &env.cancel, &rel, false, ResolveScope::ALL, accept)
                .map(|_| ())
        })
        .await
    }

    pub(super) async fn do_props_set(
        &self,
        wc: &WcRef,
        path: &str,
        name: &str,
        value: Option<&str>,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        let (path, name, value) = (path.to_owned(), name.to_owned(), value.map(str::to_owned));
        self.with_local(wc, ctx, move |db, env| {
            let (rel, _) = local_util::target(db, &path)?;
            client_local::propset(
                db,
                &env.cancel,
                &rel,
                &name,
                value.as_deref().map(str::as_bytes),
                PropSetOptions::default(),
            )
        })
        .await
    }

    pub(super) async fn do_revert(
        &self,
        wc: &WcRef,
        req: &RevertRequest,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        let paths = req.paths.clone();
        let remove_unversioned = req.remove_unversioned.unwrap_or(false);
        self.with_local(wc, ctx, move |db, env| {
            let mut rels = Vec::with_capacity(paths.len());
            let mut targets = Vec::with_capacity(paths.len());
            for p in &paths {
                let (rel, abs) = local_util::target(db, p)?;
                rels.push(rel);
                targets.push(abs);
            }
            client_local::lock::with_wc_lock(db, &env.cancel, || {
                // Collected before the revert: items that were only scheduled for addition
                // become unversioned by the revert and stay on disk, as svn keeps them.
                let mut doomed = Vec::new();
                if remove_unversioned {
                    for rel in &rels {
                        doomed.extend(local_util::unversioned_items(db, env, rel, true, false)?);
                    }
                }
                client_local::revert(
                    db,
                    &env.cancel,
                    &targets,
                    &RevertOptions { recursive: true },
                )?;
                local_util::remove_items(db, &env.cancel, &doomed)
            })
        })
        .await
    }

    pub(super) async fn do_add_paths(
        &self,
        wc: &WcRef,
        paths: &[String],
        ctx: &OpContext,
    ) -> EngineResult<()> {
        let (paths, progress) = (paths.to_vec(), ctx.progress.clone());
        self.with_local(wc, ctx, move |db, env| {
            let targets = local_util::targets(db, &paths)?;
            let opts = AddOptions {
                depth: Depth::Infinity,
                add_parents: true,
                ..AddOptions::default()
            };
            let outcome = client_local::add_reporting(db, &env.cancel, &targets, &opts, &env.add)?;
            super::progress::report_skipped_nested_wcs(&progress, &outcome.warnings);
            Ok(())
        })
        .await
    }

    pub(super) async fn do_delete_paths(
        &self,
        wc: &WcRef,
        paths: &[String],
        keep_local: bool,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        let paths = paths.to_vec();
        self.with_local(wc, ctx, move |db, env| {
            let targets = local_util::targets(db, &paths)?;
            let opts = DeleteOptions {
                keep_local,
                force: false,
                global_ignores: env.global_ignores().to_vec(),
            };
            client_local::delete(db, &env.cancel, &targets, &opts)
        })
        .await
    }

    pub(super) async fn do_move_path(
        &self,
        wc: &WcRef,
        from: &str,
        to: &str,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        let (from, to) = (from.to_owned(), to.to_owned());
        self.with_local(wc, ctx, move |db, env| {
            let (_, src) = local_util::target(db, &from)?;
            let (_, dst) = local_util::target(db, &to)?;
            client_local::move_path(db, &env.cancel, &src, &dst).map(|_| ())
        })
        .await
    }

    pub(super) async fn do_cleanup(
        &self,
        wc: &WcRef,
        req: &CleanupRequest,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        let remove_unversioned = req.remove_unversioned.unwrap_or(false);
        let remove_ignored = req.remove_ignored.unwrap_or(false);
        self.with_local(wc, ctx, move |db, env| {
            // Unreferenced pristine texts are always removed, so `vacuum_pristines` needs no step.
            let report = client_local::cleanup(db, &env.cancel)?;
            if !report.locks.needs_user_action.is_empty() {
                let dirs: Vec<String> = report
                    .locks
                    .needs_user_action
                    .iter()
                    .map(|d| {
                        if d.is_root() {
                            ".".to_owned()
                        } else {
                            d.to_string()
                        }
                    })
                    .collect();
                return Err(SvnError::new(
                    ErrorCode::WcBusy,
                    format!(
                        "the working copy is locked by another program ({}); close it and run \
                         cleanup again",
                        dirs.join(", ")
                    ),
                ));
            }
            if !(remove_unversioned || remove_ignored) {
                return Ok(());
            }
            client_local::lock::with_wc_lock(db, &env.cancel, || {
                let doomed = local_util::unversioned_items(
                    db,
                    env,
                    &RelPath::root(),
                    remove_unversioned,
                    remove_ignored,
                )?;
                local_util::remove_items(db, &env.cancel, &doomed)
            })
        })
        .await
    }

    pub(super) async fn do_upgrade_format(
        &self,
        wc: &WcRef,
        to: u8,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        ctx.check()?;
        let root = wc.root.clone();
        let current = blocking(move || Ok(super::wc_read::open_db(&root)?.format())).await?;
        if u32::from(to) == current {
            return Ok(()); // already in that format
        }
        Err(SvnError::new(
            ErrorCode::WcFormatUnsupported,
            format!(
                "Changing the working copy format from {current} to {to} is not supported yet. \
                 Check out a new working copy in the format you need (tracked in \
                 {UNSUPPORTED_ISSUE})."
            ),
        ))
    }
}

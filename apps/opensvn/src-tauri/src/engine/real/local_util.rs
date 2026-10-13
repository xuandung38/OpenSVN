//! Helpers shared by the real engine's local working copy operations (`local`) and the commit
//! path (`commit`, for `auto_add_remove`): opening the registered working copy for an
//! `svn-client` call, turning request paths into safe absolute paths, the effective ignore and
//! auto-props configuration, the per-working-copy local ignore list, and removing unversioned
//! items.
//!
//! Everything here is blocking; callers run it inside [`blocking`](super::wc_read::blocking).

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};

use svn_client::local::AddConfig;
use svn_client::status::local_status;
use svn_client::{ClientCtx, StatusOptions};
use svn_types::{
    CancelToken, Depth, ErrorCode, NodeStatus, RelPath, SvnError, WcStatus, safe_join,
};
use svn_wc::WcDb;

use crate::engine::facade::EngineResult;

/// File in the administrative directory that holds the patterns added with
/// `IgnoreScope::Local`: one pattern per line, never committed and never read by svn itself.
pub(super) const LOCAL_IGNORES_FILE: &str = "opensvn-local-ignores";

/// Name of the administrative directory.
const ADM_DIR: &str = ".svn";

/// Where the real engine reads Subversion's runtime configuration (`[miscellany]
/// global-ignores`, `enable-auto-props`, `[auto-props]`).
///
/// `None` (the default) uses svn's built-in defaults, as `svn --config-dir` pointing at an empty
/// directory would. The app points it at [`default_subversion_config_dir`] while the
/// `wc.readSubversionConfig` setting is on.
#[derive(Debug, Default)]
pub struct SvnConfigSource(RwLock<Option<PathBuf>>);

impl SvnConfigSource {
    /// Reads the configuration from `dir/config` from now on (`None`: built-in defaults).
    pub fn set_dir(&self, dir: Option<PathBuf>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = dir;
    }

    /// Directory currently configured.
    pub fn dir(&self) -> Option<PathBuf> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The `add` configuration: parsed from `config` in the configured directory, or the
    /// built-in defaults when no directory is configured or the file cannot be read.
    pub fn add_config(&self) -> AddConfig {
        self.dir()
            .and_then(|d| fs::read_to_string(d.join("config")).ok())
            .map_or_else(AddConfig::default, |text| {
                AddConfig::from_config_text(&text)
            })
    }
}

/// Subversion's per-user configuration directory: `%APPDATA%\Subversion` on Windows,
/// `~/.subversion` elsewhere.
pub fn default_subversion_config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Subversion"))
    } else {
        std::env::var_os("HOME").map(|d| PathBuf::from(d).join(".subversion"))
    }
}

/// Everything a local operation needs on its blocking thread.
pub(super) struct LocalEnv {
    /// `add` configuration; its `global_ignores` are the effective ones (configuration plus the
    /// working copy's local list).
    pub(super) add: AddConfig,
    /// Cancellation of the call.
    pub(super) cancel: CancelToken,
}

impl LocalEnv {
    /// Effective global ignore patterns (configuration plus the local list).
    pub(super) fn global_ignores(&self) -> &[String] {
        &self.add.global_ignores
    }
}

/// Builds the environment of a local operation on the working copy at `db`: the configured
/// global ignores (or svn's defaults) extended with the working copy's local ignore list.
pub(super) fn local_env(
    config: &SvnConfigSource,
    db: &WcDb,
    cancel: CancelToken,
) -> EngineResult<LocalEnv> {
    let mut add = config.add_config();
    for p in read_local_ignores(db.wcroot())? {
        if !add.global_ignores.contains(&p) {
            add.global_ignores.push(p);
        }
    }
    Ok(LocalEnv { add, cancel })
}

/// Opens the working copy registered at `root` for an `svn-client` operation (registering the
/// lock owner and recovering locks a dead process of this app left behind).
///
/// `root` must be the working copy root itself, not a directory inside one.
pub(super) fn open_wc_root(client: &ClientCtx, root: &Path) -> EngineResult<WcDb> {
    let (db, rel) = svn_client::wc::open_containing(&client.lock_owner, root)?;
    if !rel.is_root() {
        return Err(SvnError::new(
            ErrorCode::NotAWorkingCopy,
            format!(
                "'{}' is not the root of a working copy (the root is '{}')",
                root.display(),
                db.wcroot().display()
            ),
        ));
    }
    Ok(db)
}

/// Validates a `/`-separated path relative to the working copy root and returns it with its
/// absolute location. Refuses traversal (`..`), absolute and drive paths, the administrative
/// directory and symlinked intermediate directories, so no request path can reach outside the
/// working copy. The empty path is the root.
pub(super) fn target(db: &WcDb, path: &str) -> EngineResult<(RelPath, PathBuf)> {
    let rel = RelPath::new(path).map_err(|e| unsafe_request_path(path, &e))?;
    let abs = safe_join(db.wcroot(), &rel).map_err(|e| unsafe_request_path(path, &e))?;
    Ok((rel, abs))
}

/// [`target`] for each of `paths`; fails on the first bad one before anything is changed.
pub(super) fn targets(db: &WcDb, paths: &[String]) -> EngineResult<Vec<PathBuf>> {
    paths
        .iter()
        .map(|p| target(db, p).map(|(_, abs)| abs))
        .collect()
}

fn unsafe_request_path(path: &str, cause: &SvnError) -> SvnError {
    SvnError::new(
        ErrorCode::UnsafePath,
        format!("'{path}' is not a path inside the working copy"),
    )
    .with_child(cause.clone())
}

/// Error for a request value the engine refuses before touching the working copy.
pub(super) fn invalid(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::WcInvalidOperation, msg)
}

/// Path of the local ignore list of the working copy at `wcroot`.
fn local_ignores_path(wcroot: &Path) -> PathBuf {
    wcroot.join(ADM_DIR).join(LOCAL_IGNORES_FILE)
}

/// Patterns of the working copy's local ignore list (empty when there is none).
pub fn read_local_ignores(wcroot: &Path) -> EngineResult<Vec<String>> {
    let path = local_ignores_path(wcroot);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(SvnError::from_io(format!("read '{}'", path.display()), &e)),
    }
}

/// Adds `pattern` to the local ignore list (no change when it is already there). The file is
/// replaced atomically.
pub(super) fn add_local_ignore(wcroot: &Path, pattern: &str) -> EngineResult<()> {
    let mut patterns = read_local_ignores(wcroot)?;
    if patterns.iter().any(|p| p == pattern) {
        return Ok(());
    }
    patterns.push(pattern.to_owned());
    let mut text = patterns.join("\n");
    text.push('\n');
    svn_types::write_file_replacing(&local_ignores_path(wcroot), text.as_bytes())
}

/// Checks an ignore pattern the way the fake engine does: one non-empty name pattern without
/// `/`, NUL or line breaks. Returns the trimmed pattern.
pub(super) fn checked_ignore_pattern(pattern: &str) -> EngineResult<&str> {
    let p = pattern.trim();
    if p.is_empty() || p.contains(['/', '\0', '\n', '\r']) {
        return Err(invalid(format!("invalid ignore pattern {pattern:?}")));
    }
    Ok(p)
}

/// Unversioned (and, with `ignored`, ignored) items at or below `rel`, as status reports them:
/// an unversioned directory is one item. An unversioned `rel` is reported as itself when it
/// exists on disk.
pub(super) fn unversioned_items(
    db: &WcDb,
    env: &LocalEnv,
    rel: &RelPath,
    unversioned: bool,
    ignored: bool,
) -> EngineResult<Vec<RelPath>> {
    if db.read_node(rel)?.is_none() {
        let abs = safe_join(db.wcroot(), rel)?;
        return Ok(if unversioned && abs.symlink_metadata().is_ok() {
            vec![rel.clone()]
        } else {
            Vec::new()
        });
    }
    let opts = StatusOptions {
        depth: Depth::Infinity,
        show_unmodified: false,
        include_unversioned: unversioned,
        include_ignored: ignored,
    };
    let found: Vec<WcStatus> =
        local_status(db, rel, &opts, Some(env.global_ignores()), &env.cancel)?;
    Ok(found
        .into_iter()
        .filter(|s| {
            (unversioned && s.text == NodeStatus::Unversioned)
                || (ignored && s.text == NodeStatus::Ignored)
        })
        .map(|s| s.path)
        .collect())
}

/// Deletes the unversioned items `rels` from disk. Symlinks are removed, never followed; a
/// directory is removed with its content. Items that are already gone are skipped.
pub(super) fn remove_items(db: &WcDb, cancel: &CancelToken, rels: &[RelPath]) -> EngineResult<()> {
    for rel in rels {
        cancel.check()?;
        if rel.is_root() {
            continue;
        }
        let abs = safe_join(db.wcroot(), rel)?;
        let meta = match abs.symlink_metadata() {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(SvnError::from_io(format!("'{}'", abs.display()), &e)),
        };
        let removed = if meta.is_dir() {
            fs::remove_dir_all(&abs)
        } else {
            fs::remove_file(&abs)
        };
        match removed {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(SvnError::from_io(format!("remove '{}'", abs.display()), &e)),
        }
    }
    Ok(())
}

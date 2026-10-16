//! Helpers shared by the real engine's local working copy operations (`local`) and the commit
//! path (`commit`, for `auto_add_remove`): opening the registered working copy for an
//! `svn-client` call, turning request paths into safe absolute paths, the effective ignore and
//! auto-props configuration, the per-working-copy local ignore list, and removing unversioned
//! items.
//!
//! Everything here is blocking; callers run it inside [`blocking`](super::wc_read::blocking).

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};

use svn_client::local::AddConfig;
use svn_client::status::{ExternalPaths, is_wc_root_dir, local_status};
use svn_client::{ClientCtx, StatusOptions};
use svn_types::{
    CancelToken, Depth, ErrorCode, NodeStatus, RelPath, SvnError, WcStatus, safe_join,
};
use svn_wc::WcDb;
use svn_wc::conflicts::ConflictSkel;
use svn_wc::db::{Presence, sql_err};

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
///
/// The `wc.globalIgnores` setting, when not empty, replaces the `global-ignores` read from there.
#[derive(Debug, Default)]
pub struct SvnConfigSource {
    dir: RwLock<Option<PathBuf>>,
    global_ignores: RwLock<Option<Vec<String>>>,
}

impl SvnConfigSource {
    /// Reads the configuration from `dir/config` from now on (`None`: built-in defaults).
    pub fn set_dir(&self, dir: Option<PathBuf>) {
        *self.dir.write().unwrap_or_else(PoisonError::into_inner) = dir;
    }

    /// Directory currently configured.
    pub fn dir(&self) -> Option<PathBuf> {
        self.dir
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Uses these `global-ignores` patterns instead of the configured ones from now on (`None`:
    /// the configuration's own list or svn's built-in one).
    pub fn set_global_ignores(&self, patterns: Option<Vec<String>>) {
        *self
            .global_ignores
            .write()
            .unwrap_or_else(PoisonError::into_inner) = patterns;
    }

    /// The `add` configuration: parsed from `config` in the configured directory, or the
    /// built-in defaults when no directory is configured or the file cannot be read; its
    /// `global_ignores` replaced by [`Self::set_global_ignores`] when set.
    pub fn add_config(&self) -> AddConfig {
        let mut config = self
            .dir()
            .and_then(|d| fs::read_to_string(d.join("config")).ok())
            .map_or_else(AddConfig::default, |text| {
                AddConfig::from_config_text(&text)
            });
        if let Some(patterns) = self
            .global_ignores
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            config.global_ignores = patterns;
        }
        config
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
    /// Request paths are matched against `wc.db` ignoring case ([`CASE_INSENSITIVE_FS`] unless
    /// the engine overrides it).
    pub(super) case_insensitive: bool,
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
    Ok(LocalEnv {
        add,
        cancel,
        case_insensitive: CASE_INSENSITIVE_FS,
    })
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
///
/// Also refuses a path that belongs to another working copy (see [`ensure_own_path`]): an
/// external, a separate checkout nested inside this one, or anything inside either.
///
/// On case-insensitive file systems a path that `wc.db` does not know is first resolved to the
/// versioned spelling ([`resolve_case`]), so `README.MD` acts on the versioned `README.md` instead
/// of being treated as unversioned (and deleted by a revert or cleanup that removes unversioned
/// items).
pub(super) fn target(db: &WcDb, path: &str) -> EngineResult<(RelPath, PathBuf)> {
    target_with(db, path, CASE_INSENSITIVE_FS)
}

/// [`target`] with the case rule of `env` ([`LocalEnv::case_insensitive`]).
pub(super) fn target_in(db: &WcDb, env: &LocalEnv, path: &str) -> EngineResult<(RelPath, PathBuf)> {
    target_with(db, path, env.case_insensitive)
}

/// [`target_in`] for each of `paths`; fails on the first bad one before anything is changed.
pub(super) fn targets_in(
    db: &WcDb,
    env: &LocalEnv,
    paths: &[String],
) -> EngineResult<Vec<PathBuf>> {
    paths
        .iter()
        .map(|p| target_in(db, env, p).map(|(_, abs)| abs))
        .collect()
}

/// [`target`], matching `wc.db` ignoring case when `case_insensitive`.
pub(super) fn target_with(
    db: &WcDb,
    path: &str,
    case_insensitive: bool,
) -> EngineResult<(RelPath, PathBuf)> {
    let rel = RelPath::new(path).map_err(|e| unsafe_request_path(path, &e))?;
    let rel = resolve_case(db, &rel, case_insensitive)?;
    let abs = safe_join(db.wcroot(), &rel).map_err(|e| unsafe_request_path(path, &e))?;
    ensure_own_path(db, &rel)?;
    Ok((rel, abs))
}

/// [`target`] for a path that is about to be created (the destination of a move): taken as
/// spelled, so a rename that only changes case stays possible.
pub(super) fn new_target(db: &WcDb, path: &str) -> EngineResult<(RelPath, PathBuf)> {
    target_with(db, path, false)
}

/// Whether the platform's usual file systems ignore case in names (APFS/HFS+ by default, NTFS),
/// so `README.MD` and `README.md` may be the same file. Case-sensitive volumes and folders exist
/// on these platforms too (case-sensitive APFS, per-folder case sensitivity on Windows), so the
/// switch to the `wc.db` spelling is only made after the disk confirms that both names are one
/// item ([`resolve_case`]).
pub(super) const CASE_INSENSITIVE_FS: bool = cfg!(any(target_os = "macos", windows));

/// A name folded for comparison the way case-insensitive file systems compare names: Unicode
/// NFC (APFS and HFS+ also ignore normalization), then full upper-then-lower case folding.
fn fold_name(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    name.nfc()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .collect()
}

/// Paths in `wc.db` (any layer, any presence) spelled like `rel` up to case and normalization,
/// `rel` itself included when it is there. Matched component by component, so a request that
/// differs from `wc.db` in a parent folder's case is found too.
pub(super) fn case_variants(db: &WcDb, rel: &RelPath) -> EngineResult<Vec<RelPath>> {
    let mut frontier = vec![RelPath::root()];
    for component in rel.components() {
        let want = fold_name(component);
        let mut next = Vec::new();
        for parent in &frontier {
            for child in db.children(parent)? {
                if child.file_name().is_some_and(|n| fold_name(n) == want) {
                    next.push(child);
                }
            }
        }
        if next.is_empty() {
            return Ok(Vec::new());
        }
        frontier = next;
    }
    frontier.sort();
    frontier.dedup();
    Ok(frontier)
}

/// Resolves `rel` to its spelling in `wc.db` when `case_insensitive` and `wc.db` has no row for
/// it as spelled. Several versioned names that match up to case (`README.md` and `Readme.md`, as
/// a repository made on a case-sensitive system can have) are refused as ambiguous; none leaves
/// `rel` as it is (an unversioned path). One match `versioned` is used only when the disk shows
/// that nothing else can be meant ([`spelling_on_disk`]):
///
/// * neither name is on disk (a missing versioned item): `versioned`;
/// * both are, and they are the same item: `versioned`;
/// * only `rel` is: a separate item next to a missing versioned one, taken as spelled;
/// * anything else (two different items, or only the versioned one on a case-sensitive volume):
///   refused, so a request for one file never reverts, deletes or changes the other.
pub(super) fn resolve_case(
    db: &WcDb,
    rel: &RelPath,
    case_insensitive: bool,
) -> EngineResult<RelPath> {
    if !case_insensitive || rel.is_root() || db.read_node(rel)?.is_some() {
        return Ok(rel.clone());
    }
    let mut found = case_variants(db, rel)?;
    let versioned = match found.len() {
        0 => return Ok(rel.clone()),
        1 => found.remove(0),
        _ => {
            let names: Vec<String> = found.iter().map(|p| format!("'{p}'")).collect();
            return Err(invalid(format!(
                "'{rel}' matches several versioned items that differ only in case ({}); this \
                 file system cannot tell them apart, so nothing was changed",
                names.join(", ")
            )));
        }
    };
    match spelling_on_disk(db.wcroot(), rel, &versioned) {
        Spelling::Versioned => Ok(versioned),
        Spelling::AsAsked => Ok(rel.clone()),
        Spelling::Unclear => Err(invalid(format!(
            "'{rel}' is spelled differently from the versioned '{versioned}', and the disk does \
             not show them to be the same item; nothing was changed. Use the name exactly as \
             the working copy lists it"
        ))),
    }
}

/// Which spelling a request means, judged from the disk.
#[derive(Debug, PartialEq, Eq)]
enum Spelling {
    /// The `wc.db` spelling: both names are one item, or neither exists.
    Versioned,
    /// The name as asked: a separate item, and the versioned one is not on disk.
    AsAsked,
    /// Two different items, or it cannot be told: refuse.
    Unclear,
}

/// Compares the item at `asked` with the one at `versioned` (both below `wcroot`, equal up to
/// case and normalization).
fn spelling_on_disk(wcroot: &Path, asked: &RelPath, versioned: &RelPath) -> Spelling {
    let stat = |rel: &RelPath| match safe_join(wcroot, rel) {
        Ok(abs) => match abs.symlink_metadata() {
            Ok(md) => Ok(Some(md)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(_) => Err(()),
        },
        Err(_) => Err(()),
    };
    match (stat(asked), stat(versioned)) {
        (Ok(None), Ok(None)) => Spelling::Versioned,
        (Ok(Some(a)), Ok(Some(v))) if same_item(wcroot, asked, versioned, &a, &v) => {
            Spelling::Versioned
        }
        (Ok(Some(_)), Ok(None)) => Spelling::AsAsked,
        _ => Spelling::Unclear,
    }
}

/// Whether `asked` and `versioned`, which both exist, are the same item: no folder on the way
/// lists the asked spelling of a component as an entry of its own (a case-insensitive folder
/// lists only the stored name), and on Unix both resolve to the same inode.
fn same_item(
    wcroot: &Path,
    asked: &RelPath,
    versioned: &RelPath,
    asked_md: &fs::Metadata,
    versioned_md: &fs::Metadata,
) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if asked_md.dev() != versioned_md.dev() || asked_md.ino() != versioned_md.ino() {
            return false;
        }
    }
    #[cfg(not(unix))]
    let _ = (asked_md, versioned_md);
    let mut dir = wcroot.to_path_buf();
    for (a, v) in asked.components().zip(versioned.components()) {
        if a != v {
            let Ok(entries) = fs::read_dir(&dir) else {
                return false;
            };
            let listed = entries.flatten().any(|e| e.file_name().to_str() == Some(a));
            if listed {
                return false;
            }
        }
        dir.push(v);
    }
    true
}

/// Fails when `rel`, which this working copy's database does not know, belongs to another
/// working copy: when any of its prefixes, from its first component down to `rel` itself, is a
/// working copy root (a directory with its own administrative directory: a fetched external or
/// a nested checkout) or an external (an `EXTERNALS` row or an `svn:externals` definition, even
/// one not fetched yet).
///
/// Such a path is versioned data of the other working copy. svn would act on it there; this
/// engine only works on the registered working copy, so it refuses instead of treating the path
/// as unversioned (and deleting it) or skipping it silently.
pub(super) fn ensure_own_path(db: &WcDb, rel: &RelPath) -> EngineResult<()> {
    if rel.is_root() || db.read_node(rel)?.is_some() {
        return Ok(());
    }
    let externals = ExternalPaths::for_path(db, rel)?;
    let full = rel.as_str();
    let ends = full
        .match_indices('/')
        .map(|(i, _)| i)
        .chain(std::iter::once(full.len()));
    for end in ends {
        let prefix = RelPath::new(&full[..end])?;
        let kind = if externals.is_external(&prefix) {
            "an external"
        } else if is_wc_root_dir(&safe_join(db.wcroot(), &prefix)?) {
            "a nested"
        } else {
            continue;
        };
        let inner = if prefix == *rel {
            format!("'{rel}' is {kind} working copy")
        } else {
            format!("'{rel}' belongs to {kind} working copy ('{prefix}')")
        };
        return Err(invalid(format!(
            "{inner}; open that working copy to revert or change it"
        )));
    }
    Ok(())
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
///
/// Never returns what is not disposable: externals and the unversioned directories leading to
/// them (status reports those as [`NodeStatus::External`]) and separate working copies checked
/// out inside this one. A `rel` inside an external or a nested checkout is refused with an error
/// ([`ensure_own_path`]). svn's own `cleanup --remove-unversioned` deletes such a nested checkout;
/// the app deliberately keeps it, since it may hold local edits nothing else knows about.
///
/// On case-insensitive file systems an item whose name matches a versioned node up to case is
/// never returned: it is that node's file under another spelling (renamed outside svn), and
/// removing it would delete versioned data.
pub(super) fn unversioned_items(
    db: &WcDb,
    env: &LocalEnv,
    rel: &RelPath,
    unversioned: bool,
    ignored: bool,
) -> EngineResult<Vec<RelPath>> {
    let mut items = unversioned_items_as_spelled(db, env, rel, unversioned, ignored)?;
    if env.case_insensitive {
        drop_case_aliases(db, &mut items)?;
    }
    Ok(items)
}

/// Removes from `items` every path that matches a versioned node up to case.
pub(super) fn drop_case_aliases(db: &WcDb, items: &mut Vec<RelPath>) -> EngineResult<()> {
    let mut kept = Vec::with_capacity(items.len());
    for item in items.drain(..) {
        if case_variants(db, &item)?.is_empty() {
            kept.push(item);
        } else {
            tracing::warn!(path = %item, "kept: same name as a versioned item up to case");
        }
    }
    *items = kept;
    Ok(())
}

/// [`unversioned_items`] comparing names exactly as spelled.
pub(super) fn unversioned_items_as_spelled(
    db: &WcDb,
    env: &LocalEnv,
    rel: &RelPath,
    unversioned: bool,
    ignored: bool,
) -> EngineResult<Vec<RelPath>> {
    if db.read_node(rel)?.is_none() {
        // Never dispose of another working copy's data: refuse a path inside an external or a
        // nested checkout (callers have normally refused it already, through `target`).
        ensure_own_path(db, rel)?;
        let abs = safe_join(db.wcroot(), rel)?;
        let disposable = unversioned
            && abs.symlink_metadata().is_ok()
            && !is_wc_root_dir(&abs)
            && !ExternalPaths::for_path(db, rel)?.covers(rel);
        return Ok(if disposable {
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
        include_needs_lock: false,
    };
    let found: Vec<WcStatus> =
        local_status(db, rel, &opts, Some(env.global_ignores()), &env.cancel)?;
    let mut out = Vec::new();
    for s in found {
        let wanted = (unversioned && s.text == NodeStatus::Unversioned)
            || (ignored && s.text == NodeStatus::Ignored);
        if wanted && !is_wc_root_dir(&safe_join(db.wcroot(), &s.path)?) {
            out.push(s.path);
        }
    }
    Ok(out)
}

/// Removes `dir` and its content except the working copies nested anywhere inside it (a
/// directory holding an administrative directory is kept with all it contains, and so are the
/// directories leading to it). Symlinks are removed, never followed. Returns true when something
/// was kept.
fn remove_dir_keeping_wcs(dir: &Path, cancel: &CancelToken) -> EngineResult<bool> {
    cancel.check()?;
    if is_wc_root_dir(dir) {
        return Ok(true);
    }
    let mut kept = false;
    let entries = fs::read_dir(dir)
        .map_err(|e| SvnError::from_io(format!("read '{}'", dir.display()), &e))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| SvnError::from_io(format!("read '{}'", dir.display()), &e))?;
        let path = entry.path();
        let meta = match path.symlink_metadata() {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(SvnError::from_io(format!("'{}'", path.display()), &e)),
        };
        if meta.is_dir() {
            kept |= remove_dir_keeping_wcs(&path, cancel)?;
        } else {
            remove_entry(&path, false)?;
        }
    }
    if !kept {
        remove_entry(dir, true)?;
    }
    Ok(kept)
}

/// Removes one file, symlink or (empty) directory; an entry that is already gone is fine.
fn remove_entry(path: &Path, dir: bool) -> EngineResult<()> {
    let removed = if dir {
        fs::remove_dir(path)
    } else {
        // On Windows a directory symlink or junction is not a directory for `symlink_metadata`
        // but must be removed with `remove_dir` (which never touches its target).
        fs::remove_file(path).or_else(|e| {
            if cfg!(windows) && e.kind() != ErrorKind::NotFound {
                fs::remove_dir(path).map_err(|_| e)
            } else {
                Err(e)
            }
        })
    };
    match removed {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SvnError::from_io(
            format!("remove '{}'", path.display()),
            &e,
        )),
    }
}

/// Deletes the unversioned items `rels` from disk. Symlinks are removed, never followed; a
/// directory is removed with its content, except working copies nested inside it, which are
/// kept. Items that are already gone are skipped.
///
/// One item that cannot be removed does not stop the others: every item is tried and the
/// failures are reported together afterwards (the first one as the cause). Cancellation stops
/// at once.
pub(super) fn remove_items(db: &WcDb, cancel: &CancelToken, rels: &[RelPath]) -> EngineResult<()> {
    let mut failures: Vec<(RelPath, SvnError)> = Vec::new();
    for rel in rels {
        cancel.check()?;
        if rel.is_root() {
            continue;
        }
        match remove_item(db, cancel, rel) {
            Ok(()) => {}
            Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
            Err(e) => failures.push((rel.clone(), e)),
        }
    }
    let Some((_, first)) = failures.first() else {
        return Ok(());
    };
    let list: Vec<String> = failures
        .iter()
        .map(|(rel, e)| format!("'{rel}': {}", e.message))
        .collect();
    Err(SvnError::new(
        first.code,
        format!(
            "{} of {} unversioned items could not be removed: {}",
            failures.len(),
            rels.len(),
            list.join("; ")
        ),
    )
    .with_child(first.clone()))
}

fn remove_item(db: &WcDb, cancel: &CancelToken, rel: &RelPath) -> EngineResult<()> {
    let abs = safe_join(db.wcroot(), rel)?;
    let meta = match abs.symlink_metadata() {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(SvnError::from_io(format!("'{}'", abs.display()), &e)),
    };
    if meta.is_dir() {
        remove_dir_keeping_wcs(&abs, cancel).map(|_| ())
    } else {
        remove_entry(&abs, false)
    }
}

/// Conflict marker files recorded for any conflicted node of the working copy (`.mine`,
/// `.rOLD`, `.rNEW`, `.working`, `.merge-*`, `.prej`), relative to the root. They are
/// unversioned files svn wrote; auto add must never schedule them.
pub(super) fn conflict_markers(db: &WcDb) -> EngineResult<HashSet<RelPath>> {
    let conflicted: Vec<String> = db.with_read(|c| {
        let mut stmt = c
            .prepare(
                "SELECT local_relpath FROM actual_node \
                 WHERE wc_id = ?1 AND conflict_data IS NOT NULL",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([db.wc_id()], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        rows.collect::<Result<_, _>>().map_err(sql_err)
    })?;
    let mut out = HashSet::new();
    for rel in conflicted {
        let Some(rec) = db.conflict(&RelPath::new(&rel)?)? else {
            continue;
        };
        let skel = ConflictSkel::from_skel(&rec.skel)?;
        let text = skel.text().into_iter().flat_map(|t| {
            [t.original.as_ref(), t.mine.as_ref(), t.their.as_ref()]
                .into_iter()
                .flatten()
        });
        let props = skel.prop().into_iter().flat_map(|p| p.markers.iter());
        for marker in text.chain(props) {
            // A marker that is not a valid working copy path cannot be a status item either.
            if let Ok(m) = RelPath::new(marker) {
                out.insert(m);
            }
        }
    }
    Ok(out)
}

/// Whether the missing versioned node `rel` may be scheduled for deletion by auto add/remove:
/// only a node that exists in BASE (not one only scheduled for addition or copied, which svn
/// refuses to commit as missing) and that is not part of an interrupted checkout or update.
///
/// A folder recorded as incomplete (`presence = 'incomplete'`) and everything below it may
/// simply not have been fetched yet: its "missing" content is not a user deletion, so it is
/// never deleted (issue #99).
///
/// A file external is never deleted: its BASE row is the external's source, so deleting it would
/// delete that source from the repository. svn refuses to delete externals (E155030); a missing
/// one is restored by the next update.
pub(super) fn missing_is_deletable(db: &WcDb, rel: &RelPath) -> EngineResult<bool> {
    let Some(node) = db.read_node(rel)? else {
        return Ok(false);
    };
    if node.op_depth != 0 || node.presence != Presence::Normal || node.file_external {
        return Ok(false);
    }
    let mut cur = Some(rel.clone());
    while let Some(p) = cur {
        if db
            .read_base(&p)?
            .is_some_and(|b| b.presence == Presence::Incomplete)
        {
            return Ok(false);
        }
        cur = p.parent();
    }
    Ok(true)
}

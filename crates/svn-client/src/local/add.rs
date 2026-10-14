//! `svn add`: schedule unversioned files and directories for addition.

use std::fs;
use std::path::Path;

use svn_types::{CancelToken, Depth, ErrorCode, NodeKind, RelPath, Result, SvnError};
use svn_wc::WcDb;

use super::autoprops::{AddConfig, detect_binary_mimetype, merge, props_from_property};
use super::ignore::is_ignored;
use super::lock::write_op;
use super::paths::{abs_of, rel_of};
use super::props::{PropSetOptions, propset_locked};
use super::state::{is_versioned, node_info};
use crate::status::is_wc_root_dir;

/// Options of [`add`].
#[derive(Debug, Clone, Copy)]
pub struct AddOptions {
    /// `Empty` adds only the target; `Infinity` (or anything deeper than `Files`) recurses.
    pub depth: Depth,
    /// Descend into directories that are already versioned and add their unversioned content.
    pub force: bool,
    /// Do not apply ignore rules while recursing.
    pub no_ignore: bool,
    /// Do not apply auto-props or mime type / executable detection.
    pub no_autoprops: bool,
    /// Add missing versioned ancestors too.
    pub add_parents: bool,
}

impl Default for AddOptions {
    fn default() -> Self {
        Self {
            depth: Depth::Infinity,
            force: false,
            no_ignore: false,
            no_autoprops: false,
            add_parents: false,
        }
    }
}

fn invalid(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::WcInvalidOperation, msg)
}

fn io_err(path: &Path, e: &std::io::Error) -> SvnError {
    SvnError::from_io(format!("'{}'", path.display()), e)
}

/// `SVN_ERR_WC_PATH_FOUND` (W150002): `abs` is the root of another working copy.
fn nested_wc(abs: &Path) -> SvnError {
    invalid(format!(
        "'{}' is already under version control",
        abs.display()
    ))
    .with_svn_code(150_002)
}

/// Result of [`add_reporting`].
#[derive(Debug, Default)]
pub struct AddOutcome {
    /// Every newly scheduled path, in the order it was added.
    pub added: Vec<RelPath>,
    /// svn's W150002 warnings: directories met while recursing that are the root of another
    /// working copy (a separate checkout or an external). They are skipped with everything they
    /// contain and stay unversioned, as `svn add` leaves them.
    pub warnings: Vec<SvnError>,
}

/// Adds `targets` (absolute paths inside the working copy) and returns every newly scheduled
/// path in the order it was added. Nested working copies met while recursing are skipped
/// silently; [`add_reporting`] also returns a warning for each.
pub fn add(
    db: &WcDb,
    cancel: &CancelToken,
    targets: &[std::path::PathBuf],
    opts: &AddOptions,
    config: &AddConfig,
) -> Result<Vec<RelPath>> {
    add_reporting(db, cancel, targets, opts, config).map(|o| o.added)
}

/// [`add`], also returning the W150002 warning of every nested working copy that was skipped.
///
/// A target that is itself the root of another working copy (or lies below one, through
/// `add_parents`) is refused with W150002 before anything is added.
pub fn add_reporting(
    db: &WcDb,
    cancel: &CancelToken,
    targets: &[std::path::PathBuf],
    opts: &AddOptions,
    config: &AddConfig,
) -> Result<AddOutcome> {
    write_op(db, cancel, || {
        let mut rels = Vec::with_capacity(targets.len());
        for t in targets {
            let rel = rel_of(db, t)?;
            // Refuse every target first so a refusal leaves the whole call without effect.
            let mut cur = Some(rel.clone());
            while let Some(r) = cur {
                if r.is_root() || is_versioned(db, &r)? {
                    break;
                }
                let abs = abs_of(db, &r);
                if is_wc_root_dir(&abs) {
                    return Err(nested_wc(&abs));
                }
                cur = r.parent();
            }
            rels.push(rel);
        }
        let mut out = AddOutcome::default();
        for rel in &rels {
            cancel.check()?;
            add_target(db, cancel, rel, opts, config, &mut out)?;
        }
        Ok(out)
    })
}

fn add_target(
    db: &WcDb,
    cancel: &CancelToken,
    rel: &RelPath,
    opts: &AddOptions,
    config: &AddConfig,
    out: &mut AddOutcome,
) -> Result<()> {
    let abs = abs_of(db, rel);
    let meta = fs::symlink_metadata(&abs).map_err(|e| io_err(&abs, &e))?;
    if rel.is_root() {
        return Err(invalid("the working copy root is already versioned"));
    }
    if is_versioned(db, rel)? {
        let info = node_info(db, rel)?;
        let deleted = info.schedule == super::state::Schedule::Deleted;
        if opts.force && meta.is_dir() && !deleted {
            return add_children(db, cancel, rel, opts, config, out);
        }
        if !deleted {
            return Err(invalid(format!("'{rel}' is already under version control")));
        }
    }
    if let Some(parent) = rel.parent()
        && !is_versioned(db, &parent)?
    {
        if !opts.add_parents {
            return Err(invalid(format!(
                "can't add '{rel}': its parent directory is not under version control"
            )));
        }
        add_target(db, cancel, &parent, &shallow(opts), config, out)?;
    }
    add_one(db, rel, &abs, &meta, opts, config)?;
    out.added.push(rel.clone());
    if meta.is_dir() && !matches!(opts.depth, Depth::Empty | Depth::Exclude) {
        add_children(db, cancel, rel, opts, config, out)?;
    }
    Ok(())
}

fn shallow(opts: &AddOptions) -> AddOptions {
    AddOptions {
        depth: Depth::Empty,
        ..*opts
    }
}

fn add_children(
    db: &WcDb,
    cancel: &CancelToken,
    dir: &RelPath,
    opts: &AddOptions,
    config: &AddConfig,
    out: &mut AddOutcome,
) -> Result<()> {
    let abs = abs_of(db, dir);
    let mut names: Vec<(String, fs::Metadata)> = Vec::new();
    for entry in fs::read_dir(&abs).map_err(|e| io_err(&abs, &e))? {
        let entry = entry.map_err(|e| io_err(&abs, &e))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name == svn_wc::db::ADM_DIR {
            continue;
        }
        let meta = fs::symlink_metadata(entry.path()).map_err(|e| io_err(&entry.path(), &e))?;
        names.push((name, meta));
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, meta) in names {
        cancel.check()?;
        let Ok(child) = dir.child(&name) else {
            continue;
        };
        if is_versioned(db, &child)? {
            if meta.is_dir() && matches!(opts.depth, Depth::Infinity | Depth::Unknown) {
                add_children(db, cancel, &child, opts, config, out)?;
            }
            continue;
        }
        let child_abs = abs_of(db, &child);
        if meta.is_dir() && is_wc_root_dir(&child_abs) {
            // Another working copy (a separate checkout or an external): svn warns with
            // W150002 and leaves it, and all it contains, unversioned.
            out.warnings.push(nested_wc(&child_abs));
            continue;
        }
        if !opts.no_ignore && is_ignored(db, dir, &name, &config.global_ignores)? {
            continue;
        }
        add_one(db, &child, &child_abs, &meta, opts, config)?;
        out.added.push(child.clone());
        if meta.is_dir() && matches!(opts.depth, Depth::Infinity | Depth::Unknown) {
            add_children(db, cancel, &child, opts, config, out)?;
        }
    }
    Ok(())
}

/// Schedules one node and applies its automatic properties.
fn add_one(
    db: &WcDb,
    rel: &RelPath,
    abs: &Path,
    meta: &fs::Metadata,
    opts: &AddOptions,
    config: &AddConfig,
) -> Result<()> {
    let is_symlink = meta.file_type().is_symlink();
    let kind = if meta.is_dir() {
        NodeKind::Dir
    } else if meta.is_file() || is_symlink {
        NodeKind::File
    } else {
        return Err(invalid(format!(
            "'{rel}' is not a file, directory or symlink"
        )));
    };
    db.op_add(rel, kind, None, &[])?;
    if opts.no_autoprops {
        return Ok(());
    }
    let mut props: Vec<(String, String)> = Vec::new();
    if kind == NodeKind::File {
        let name = rel.file_name().unwrap_or_default();
        merge(&mut props, &config.props_for(name));
        // Inheritable `svn:auto-props` of the directories above, root-most first.
        let mut dirs: Vec<RelPath> = Vec::new();
        let mut cur = rel.parent();
        while let Some(d) = cur {
            cur = d.parent();
            dirs.push(d);
        }
        for d in dirs.iter().rev() {
            if let Some((_, v)) = db
                .actual_props(d)?
                .into_iter()
                .find(|(k, _)| k.as_slice() == b"svn:auto-props")
            {
                merge(
                    &mut props,
                    &props_from_property(&String::from_utf8_lossy(&v), name),
                );
            }
        }
        if is_symlink {
            merge(&mut props, &[("svn:special".into(), "*".into())]);
        } else {
            if !props.iter().any(|(k, _)| k == "svn:mime-type")
                && let Some(m) = detect_binary_mimetype(abs)
            {
                props.push(("svn:mime-type".into(), m.into()));
            }
            if !props.iter().any(|(k, _)| k == "svn:executable") && is_executable(meta) {
                props.push(("svn:executable".into(), "*".into()));
            }
        }
    }
    for (k, v) in props {
        propset_locked(
            db,
            rel,
            &k,
            Some(v.as_bytes()),
            PropSetOptions { skip_checks: true },
        )?;
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o100 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

//! Property commands: propset, propdel, propget, proplist and `svn:ignore` helpers.

use std::fs;

use svn_types::{CancelToken, ErrorCode, NodeKind, RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::translate::{EolStyle, check_eol_consistency};
use svn_wc::workqueue::WorkItem;

use super::lock::write_op;
use super::paths::abs_of;
use super::state::{Schedule, node_info};
use crate::wc::skels;

const FILE_ONLY: &[&str] = &[
    "svn:executable",
    "svn:keywords",
    "svn:eol-style",
    "svn:mime-type",
    "svn:needs-lock",
    "svn:special",
];
const DIR_ONLY: &[&str] = &[
    "svn:ignore",
    "svn:global-ignores",
    "svn:externals",
    "svn:auto-props",
    "svn:inheritable-auto-props",
    "svn:inheritable-global-ignores",
];

fn invalid(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::WcInvalidOperation, msg)
}

/// `svn_prop_name_is_valid`: starts with a letter, `:` or `_`, then letters, digits and `:_-.`.
pub fn prop_name_is_valid(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == ':' || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-' | '.'))
}

/// Canonicalizes a Subversion-namespace property value the way `svn propset` does: boolean
/// properties become `*`, line endings are LF, and list properties end with a newline.
pub fn canonicalize_value(name: &str, value: &[u8]) -> Vec<u8> {
    if matches!(name, "svn:executable" | "svn:needs-lock" | "svn:special") {
        return b"*".to_vec();
    }
    if !name.starts_with("svn:") {
        return value.to_vec();
    }
    let mut out = Vec::with_capacity(value.len() + 1);
    let mut i = 0;
    while i < value.len() {
        match value[i] {
            b'\r' => {
                out.push(b'\n');
                if value.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    let list_prop = matches!(
        name,
        "svn:ignore"
            | "svn:global-ignores"
            | "svn:externals"
            | "svn:auto-props"
            | "svn:inheritable-auto-props"
            | "svn:inheritable-global-ignores"
    );
    if list_prop && !out.is_empty() && out.last() != Some(&b'\n') {
        out.push(b'\n');
    }
    if matches!(name, "svn:eol-style" | "svn:mime-type" | "svn:keywords") {
        while out.last().is_some_and(u8::is_ascii_whitespace) {
            out.pop();
        }
    }
    out
}

/// Options of [`propset`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PropSetOptions {
    /// Skip the file/directory applicability and line-ending checks.
    pub skip_checks: bool,
}

/// Sets (`Some`) or deletes (`None`) property `name` on `rel` under the write lock.
pub fn propset(
    db: &WcDb,
    cancel: &CancelToken,
    rel: &RelPath,
    name: &str,
    value: Option<&[u8]>,
    opts: PropSetOptions,
) -> Result<()> {
    write_op(db, cancel, || propset_locked(db, rel, name, value, opts))
}

/// [`propset`] for callers that already hold the write lock and run the queue themselves.
pub(crate) fn propset_locked(
    db: &WcDb,
    rel: &RelPath,
    name: &str,
    value: Option<&[u8]>,
    opts: PropSetOptions,
) -> Result<()> {
    if !prop_name_is_valid(name) {
        return Err(invalid(format!("'{name}' is not a valid property name")));
    }
    let info = node_info(db, rel)?;
    if info.schedule == Schedule::Deleted {
        return Err(invalid(format!(
            "can't set properties on '{rel}': it is scheduled for deletion"
        )));
    }
    let is_dir = info.node.kind == NodeKind::Dir;
    if !opts.skip_checks {
        if is_dir && FILE_ONLY.contains(&name) {
            return Err(invalid(format!(
                "cannot set '{name}' on a directory ('{rel}')"
            )));
        }
        if !is_dir && DIR_ONLY.contains(&name) {
            return Err(invalid(format!("cannot set '{name}' on a file ('{rel}')")));
        }
    }
    let value = value.map(|v| canonicalize_value(name, v));
    if !opts.skip_checks
        && name == "svn:eol-style"
        && let Some(v) = &value
        && !is_dir
    {
        if EolStyle::from_prop(Some(v)) == EolStyle::Unknown {
            return Err(invalid(format!(
                "unrecognized line ending style '{}' for '{rel}'",
                String::from_utf8_lossy(v)
            )));
        }
        let abs = abs_of(db, rel);
        if abs.is_file() {
            let bytes = fs::read(&abs)
                .map_err(|e| SvnError::from_io(format!("read '{}'", abs.display()), &e))?;
            check_eol_consistency(&bytes).map_err(|_| {
                invalid(format!(
                    "file '{rel}' has inconsistent newlines; fix them before setting svn:eol-style"
                ))
            })?;
        }
    }
    let mut props = db.actual_props(rel)?;
    let old = props
        .iter()
        .find(|(k, _)| k.as_slice() == name.as_bytes())
        .map(|(_, v)| v.clone());
    props.retain(|(k, _)| k.as_slice() != name.as_bytes());
    // Changing how the file is translated invalidates its stat cache, so the next status reads
    // the file again (`propset_internal`'s `clear_recorded_info`).
    let clear_recorded_info = !is_dir
        && matches!(name, "svn:eol-style" | "svn:keywords")
        && old.as_deref() != value.as_deref();
    if let Some(v) = value {
        props.push((name.as_bytes().to_vec(), v));
    }
    props.sort();
    let mut work = Vec::new();
    if !is_dir && matches!(name, "svn:executable" | "svn:needs-lock") {
        work.push(WorkItem::SyncFileFlags {
            relpath: rel.as_str().to_owned(),
        });
    }
    db.with_txn(|t| {
        t.set_props(rel, &props)?;
        if clear_recorded_info {
            t.record_fileinfo(rel, Some(-1), Some(0))?;
        }
        t.queue_work(&skels(&work))
    })
}

/// Value of property `name` in the working copy (local changes included).
pub fn propget(db: &WcDb, rel: &RelPath, name: &str) -> Result<Option<Vec<u8>>> {
    node_info(db, rel)?;
    Ok(db
        .actual_props(rel)?
        .into_iter()
        .find(|(k, _)| k.as_slice() == name.as_bytes())
        .map(|(_, v)| v))
}

/// All working properties of `rel`.
pub fn proplist(db: &WcDb, rel: &RelPath) -> Result<Vec<(String, Vec<u8>)>> {
    node_info(db, rel)?;
    Ok(db
        .actual_props(rel)?
        .into_iter()
        .map(|(k, v)| (String::from_utf8_lossy(&k).into_owned(), v))
        .collect())
}

/// Adds `patterns` to `svn:ignore` of directory `dir` (the change needs a commit).
pub fn ignore_add(
    db: &WcDb,
    cancel: &CancelToken,
    dir: &RelPath,
    patterns: &[String],
) -> Result<()> {
    write_op(db, cancel, || {
        let current = propget(db, dir, "svn:ignore")?.unwrap_or_default();
        let mut text = String::from_utf8_lossy(&current).into_owned();
        for p in patterns {
            if p.is_empty() || p.contains('\n') {
                return Err(invalid(format!("invalid ignore pattern {p:?}")));
            }
            if !text.lines().any(|l| l == p) {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(p);
                text.push('\n');
            }
        }
        propset_locked(
            db,
            dir,
            "svn:ignore",
            Some(text.as_bytes()),
            PropSetOptions::default(),
        )
    })
}

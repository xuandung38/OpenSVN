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

/// Subversion-namespace properties that apply to any node (`svn_prop_is_known_svn_node_prop`).
const ANY_NODE: &[&str] = &["svn:mergeinfo"];

fn invalid(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::WcInvalidOperation, msg)
}

/// Whether `name` is a node property svn knows (`svn propset` refuses other `svn:` names
/// unless `--force` is given).
pub fn is_known_svn_prop(name: &str) -> bool {
    FILE_ONLY.contains(&name) || DIR_ONLY.contains(&name) || ANY_NODE.contains(&name)
}

/// `svn_mime_type_validate`: a media type with a `/`, no white space, control or special
/// characters before the parameters, and no control characters at all.
pub fn validate_mime_type(value: &str) -> std::result::Result<(), String> {
    const TSPECIALS: &str = "()<>@,;:\\\"/[]?=";
    let len = value.find([';', ' ']).unwrap_or(value.len());
    let media = &value[..len];
    if len == 0 {
        return Err(format!("MIME type '{value}' has empty media type"));
    }
    let Some(slash) = media.find('/') else {
        return Err(format!("MIME type '{value}' does not contain '/'"));
    };
    for (i, c) in media.char_indices() {
        if i != slash
            && (!c.is_ascii()
                || c.is_ascii_control()
                || c.is_ascii_whitespace()
                || TSPECIALS.contains(c))
        {
            return Err(format!(
                "MIME type '{value}' contains invalid character '{c}' in media type"
            ));
        }
    }
    if let Some(c) = value.chars().find(|c| c.is_control() && *c != '\t') {
        return Err(format!(
            "MIME type '{value}' contains invalid character '0x{:02x}' in postfix",
            u32::from(c)
        ));
    }
    Ok(())
}

/// `svn_mime_type_is_binary`: anything but `text/*` and the two X bitmap types.
pub fn mime_type_is_binary(value: &str) -> bool {
    let media = value.split(';').next().unwrap_or("").trim();
    !(media.starts_with("text/") || media == "image/x-xbitmap" || media == "image/x-xpixmap")
}

/// `svn_mergeinfo_parse`: lines `/path:ranges`, ranges `N`, `N-M` (`N < M`) or either with a
/// trailing `*`, revisions from 1.
pub fn validate_mergeinfo(value: &str) -> std::result::Result<(), String> {
    let rev = |w: &str| -> std::result::Result<u64, String> {
        match w.parse::<u64>() {
            Ok(0) => Err(format!(
                "invalid revision number '0' found in range list '{w}'"
            )),
            Ok(n) => Ok(n),
            Err(_) => Err(format!("invalid revision number found parsing '{w}'")),
        }
    };
    for line in value.lines() {
        if line.is_empty() {
            continue;
        }
        let Some((path, ranges)) = line.rsplit_once(':') else {
            return Err(format!("pathname not terminated by ':' in '{line}'"));
        };
        if !path.starts_with('/') {
            return Err(format!("mergeinfo path '{path}' is not absolute"));
        }
        if ranges.trim().is_empty() {
            return Err(format!(
                "mergeinfo for '{path}' maps to an empty revision range"
            ));
        }
        for range in ranges.split(',') {
            let range = range.trim().strip_suffix('*').unwrap_or(range.trim());
            match range.split_once('-') {
                Some((a, b)) => {
                    let (a, b) = (rev(a)?, rev(b)?);
                    if a >= b {
                        return Err(format!("unable to parse reversed revision range '{a}-{b}'"));
                    }
                }
                None => {
                    rev(range)?;
                }
            }
        }
    }
    Ok(())
}

/// The checks `svn_wc_canonicalize_svn_prop` and `svn propset` make on a Subversion-namespace
/// property before it is stored (the line-ending check of `svn:eol-style` is made by the caller,
/// which has the file). `value` is canonical; `props` are the node's current properties.
fn check_svn_prop(
    rel: &RelPath,
    name: &str,
    value: &[u8],
    is_dir: bool,
    props: &[(Vec<u8>, Vec<u8>)],
) -> Result<()> {
    if !is_known_svn_prop(name) {
        return Err(
            invalid(format!("'{name}' is not a valid svn: property name")).with_svn_code(195_011),
        );
    }
    if is_dir && FILE_ONLY.contains(&name) {
        return Err(invalid(format!(
            "cannot set '{name}' on a directory ('{rel}')"
        )));
    }
    if !is_dir && DIR_ONLY.contains(&name) {
        return Err(invalid(format!("cannot set '{name}' on a file ('{rel}')")));
    }
    let text = |what: &str| {
        std::str::from_utf8(value).map_err(|_| {
            invalid(format!(
                "the value of '{what}' on '{rel}' is not UTF-8 text"
            ))
        })
    };
    match name {
        "svn:mime-type" => validate_mime_type(text(name)?).map_err(invalid)?,
        "svn:eol-style" => {
            if EolStyle::from_prop(Some(value)) == EolStyle::Unknown {
                return Err(invalid(format!(
                    "unrecognized line ending style '{}' for '{rel}'",
                    String::from_utf8_lossy(value)
                )));
            }
            let binary = props
                .iter()
                .find(|(k, _)| k.as_slice() == b"svn:mime-type")
                .is_some_and(|(_, v)| mime_type_is_binary(&String::from_utf8_lossy(v)));
            if binary {
                return Err(invalid(format!(
                    "cannot set 'svn:eol-style' on a file ('{rel}') with binary mime type"
                ))
                .with_svn_code(200_009));
            }
        }
        "svn:externals" => {
            crate::externals::parse_externals(rel.as_str(), value)?;
        }
        "svn:mergeinfo" => {
            validate_mergeinfo(text(name)?)
                .map_err(|e| invalid(format!("invalid svn:mergeinfo on '{rel}': {e}")))?;
        }
        _ => {}
    }
    Ok(())
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
    let value = value.map(|v| canonicalize_value(name, v));
    let mut props = db.actual_props(rel)?;
    // Only a value that is set is checked: deleting any property is always allowed.
    if !opts.skip_checks
        && name.starts_with("svn:")
        && let Some(v) = &value
    {
        check_svn_prop(rel, name, v, is_dir, &props)?;
    }
    if !opts.skip_checks && name == "svn:eol-style" && value.is_some() && !is_dir {
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

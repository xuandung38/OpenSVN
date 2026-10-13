//! `diff`: working copy against BASE, in the format of `svn diff`.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use svn_diff::{DiffError, UnifiedOptions, diff_as_binary_by_mime, native_eol, unified};
use svn_types::{
    CancelToken, Depth, ErrorCode, NodeKind, NodeStatus, PropStatus, RelPath, Result, SvnError,
    WcStatus,
};
use svn_wc::db::{Node, Props, WcDb};
use svn_wc::pristine::Sha1Digest;

use crate::ctx::ClientCtx;
use crate::status::{StatusOptions, local_status};
use crate::wc::inspect;

/// Options of [`diff`].
#[derive(Debug, Clone)]
pub struct DiffOptions {
    /// Context lines around changes.
    pub context: usize,
    /// How deep to look.
    pub depth: Depth,
    /// Paths are printed relative to this directory (like running `svn diff` inside it).
    pub relative_to: RelPath,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            context: svn_diff::DEFAULT_CONTEXT_SIZE,
            depth: Depth::Infinity,
            relative_to: RelPath::root(),
        }
    }
}

fn map_diff_err(e: DiffError) -> SvnError {
    match e {
        DiffError::Cancelled => SvnError::new(ErrorCode::Cancelled, "diff cancelled"),
        DiffError::TooComplex => SvnError::new(ErrorCode::Internal, e.to_string()),
    }
}

fn banner(path: &str, eol: &str) -> String {
    format!("Index: {path}{eol}{}{eol}", "=".repeat(67))
}

fn read_pristine(db: &WcDb, node: &Node) -> Result<Vec<u8>> {
    let Some(sha1) = node
        .checksum
        .as_deref()
        .and_then(Sha1Digest::from_svn_string)
    else {
        return Ok(Vec::new());
    };
    let store = db.pristine_store();
    let mut f = store.open_text(&sha1)?;
    let mut out = Vec::new();
    f.read_to_end(&mut out)
        .map_err(|e| SvnError::from_io("cannot read the pristine text", &e))?;
    Ok(out)
}

/// `Added: name` / `Modified: name` / `Deleted: name` sections, sorted by property name.
fn prop_section(
    path: &str,
    old: &Props,
    new: &Props,
    opts: &DiffOptions,
    eol: &str,
    cancel: &CancelToken,
) -> Result<String> {
    let o: BTreeMap<_, _> = old.iter().cloned().collect();
    let n: BTreeMap<_, _> = new.iter().cloned().collect();
    let mut names: Vec<&Vec<u8>> = o.keys().chain(n.keys()).collect();
    names.sort();
    names.dedup();
    let mut body = String::new();
    for name in names {
        let (ov, nv) = (o.get(name), n.get(name));
        if ov == nv {
            continue;
        }
        let label = match (ov, nv) {
            (None, _) => "Added",
            (_, None) => "Deleted",
            _ => "Modified",
        };
        body.push_str(&format!("{label}: {}{eol}", String::from_utf8_lossy(name)));
        let (a, b) = (
            ov.map(Vec::as_slice).unwrap_or_default(),
            nv.map(Vec::as_slice).unwrap_or_default(),
        );
        if a.is_empty() && b.is_empty() {
            continue;
        }
        let u = unified(
            a,
            b,
            &UnifiedOptions {
                context_size: opts.context,
                hunk_delimiter: "##".to_owned(),
                eol: eol.to_owned(),
                ..UnifiedOptions::default()
            },
            cancel,
        )
        .map_err(map_diff_err)?;
        let text = String::from_utf8_lossy(&u).into_owned();
        // Property hunks have no `---`/`+++` header lines and their own end-of-text marker.
        let hunks: String = text.split_inclusive('\n').skip(2).collect();
        body.push_str(&hunks.replace(
            "\\ No newline at end of file",
            "\\ No newline at end of property",
        ));
    }
    if body.is_empty() {
        return Ok(body);
    }
    Ok(format!(
        "{eol}Property changes on: {path}{eol}{}{eol}{body}",
        "_".repeat(67)
    ))
}

fn display_path(rel: &RelPath, opts: &DiffOptions) -> String {
    match crate::wc::strip_rel(rel, &opts.relative_to) {
        Some(p) if p.is_root() => ".".to_owned(),
        Some(p) => p.as_str().to_owned(),
        None => rel.as_str().to_owned(),
    }
}

fn diff_node(db: &WcDb, s: &WcStatus, opts: &DiffOptions, cancel: &CancelToken) -> Result<Vec<u8>> {
    let eol = native_eol();
    let shown = display_path(&s.path, opts);
    let cur = db.read_node(&s.path)?;
    let base = db.read_base(&s.path)?.filter(|b| b.presence.is_present());
    // What the working node is compared with: the copy source for a copied node (like `svn
    // diff` without `--show-copies-as-adds`), else BASE unless the node is a plain addition.
    let copy_source = cur
        .as_ref()
        .filter(|c| c.op_depth > 0 && c.presence.is_present() && c.revision.is_some());
    let old_node = match copy_source {
        Some(c) => Some(c),
        None if s.text == NodeStatus::Added => None,
        None => base.as_ref(),
    };
    let old_props = match old_node {
        Some(n) => n.properties.clone().unwrap_or_default(),
        None => Vec::new(),
    };
    let new_props = if s.text == NodeStatus::Deleted {
        Vec::new()
    } else {
        db.actual_props(&s.path)?
    };
    let prop_changed = {
        let mut o = old_props.clone();
        let mut n = new_props.clone();
        o.sort();
        n.sort();
        o != n
    };
    let text_changed = s.kind == NodeKind::File
        && match (copy_source, s.text) {
            (Some(c), NodeStatus::Added | NodeStatus::Replaced | NodeStatus::Modified) => {
                inspect::text_modified(db, &s.path, c)?
            }
            (_, t) => matches!(
                t,
                NodeStatus::Modified
                    | NodeStatus::Added
                    | NodeStatus::Deleted
                    | NodeStatus::Replaced
                    | NodeStatus::Conflicted
            ),
        };
    if !text_changed && !prop_changed {
        return Ok(Vec::new());
    }
    let rev = old_node.and_then(|n| n.revision).unwrap_or(0);
    let old_label = if old_node.is_some() {
        format!("{shown}\t(revision {rev})")
    } else {
        format!("{shown}\t(nonexistent)")
    };
    let new_label = if s.text == NodeStatus::Deleted {
        format!("{shown}\t(nonexistent)")
    } else {
        format!("{shown}\t(working copy)")
    };
    let mut out = banner(&shown, eol);
    let mime_src = if s.text == NodeStatus::Deleted {
        &old_props
    } else {
        &new_props
    };
    let mime =
        inspect::prop_value(mime_src, "svn:mime-type").and_then(|v| std::str::from_utf8(v).ok());
    if text_changed && diff_as_binary_by_mime(mime) {
        out.push_str(&format!(
            "Cannot display: file marked as a binary type.{eol}svn:mime-type = {}{eol}",
            mime.unwrap_or_default()
        ));
    } else {
        let mut printed_header = false;
        if text_changed {
            let old = match old_node {
                Some(n) => read_pristine(db, n)?,
                None => Vec::new(),
            };
            let new = if s.text == NodeStatus::Deleted {
                Vec::new()
            } else {
                inspect::working_repo_normal(db, &s.path)?
            };
            let u = unified(
                &old,
                &new,
                &UnifiedOptions {
                    context_size: opts.context,
                    original_header: old_label.clone(),
                    modified_header: new_label.clone(),
                    eol: eol.to_owned(),
                    ..UnifiedOptions::default()
                },
                cancel,
            )
            .map_err(map_diff_err)?;
            printed_header = !u.is_empty();
            out.push_str(&String::from_utf8_lossy(&u));
        }
        if prop_changed && !printed_header {
            out.push_str(&format!("--- {old_label}{eol}+++ {new_label}{eol}"));
        }
    }
    if prop_changed {
        out.push_str(&prop_section(
            &shown, &old_props, &new_props, opts, eol, cancel,
        )?);
    }
    Ok(out.into_bytes())
}

/// Sort key that orders paths depth first, children by byte order, parents after children.
fn post_order_key(p: &RelPath) -> Vec<(u8, Vec<u8>)> {
    let mut key: Vec<(u8, Vec<u8>)> = if p.is_root() {
        Vec::new()
    } else {
        p.as_str()
            .split('/')
            .map(|c| (0, c.as_bytes().to_vec()))
            .collect()
    };
    key.push((1, Vec::new()));
    key
}

/// Differences between the working files below `rel` and their pristine texts.
pub fn diff_local(
    db: &WcDb,
    rel: &RelPath,
    opts: &DiffOptions,
    cancel: &CancelToken,
) -> Result<Vec<u8>> {
    let status = local_status(
        db,
        rel,
        &StatusOptions {
            depth: opts.depth,
            show_unmodified: false,
            include_unversioned: false,
            include_ignored: false,
        },
        None,
        cancel,
    )?;
    let mut status = status;
    // `svn diff` walks each directory's children in byte order and reports a directory's own
    // property changes after everything below it.
    status.sort_by_cached_key(|s| post_order_key(&s.path));
    let mut out = Vec::new();
    for s in status {
        cancel.check()?;
        let interesting = matches!(
            s.text,
            NodeStatus::Modified
                | NodeStatus::Added
                | NodeStatus::Deleted
                | NodeStatus::Replaced
                | NodeStatus::Conflicted
        ) || s.props == PropStatus::Modified
            || s.props == PropStatus::Conflicted;
        if interesting {
            out.extend(diff_node(db, &s, opts, cancel)?);
        }
    }
    Ok(out)
}

/// `svn diff` of the working copy item at `path` against BASE.
pub async fn diff(
    ctx: &ClientCtx,
    path: &Path,
    opts: &DiffOptions,
    cancel: &CancelToken,
) -> Result<Vec<u8>> {
    let (owner, p, opts, cancel) = (
        ctx.lock_owner.clone(),
        path.to_path_buf(),
        opts.clone(),
        cancel.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let (db, rel) = crate::wc::open_containing(&owner, &p)?;
        diff_local(&db, &rel, &opts, &cancel)
    })
    .await
    .map_err(|e| SvnError::new(ErrorCode::Internal, format!("worker failed: {e}")))?
}

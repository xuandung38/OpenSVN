//! `svn resolve`: mark conflicts resolved, optionally taking one side.

use svn_types::{CancelToken, ErrorCode, RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::conflicts::ConflictSkel;
use svn_wc::db::ConflictRecord;
use svn_wc::workqueue::WorkItem;

use super::lock::write_op;
use super::state::subtree;
use crate::wc::skels;

/// Which version of the conflicted content to keep (`--accept`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveChoice {
    /// Keep the working file / properties as they are (`working`, also used after manual edit).
    Working,
    /// The common ancestor (`base`): the original text, or the properties before the update.
    Base,
    /// The user's whole version (`mine-full`).
    MineFull,
    /// The incoming whole version (`theirs-full`).
    TheirsFull,
}

/// Which kinds of conflict to resolve.
#[derive(Debug, Clone, Copy)]
pub struct ResolveScope {
    /// Text conflicts.
    pub text: bool,
    /// Property conflicts.
    pub props: bool,
    /// Tree conflicts.
    pub tree: bool,
}

impl ResolveScope {
    /// All kinds.
    pub const ALL: Self = Self {
        text: true,
        props: true,
        tree: true,
    };
}

fn invalid(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::WcInvalidOperation, msg)
}

/// Resolves the conflicts of `rel` (and, when `recursive`, of its subtree) that match `scope`.
/// Returns the paths that had at least one conflict resolved.
pub fn resolve(
    db: &WcDb,
    cancel: &CancelToken,
    rel: &RelPath,
    recursive: bool,
    scope: ResolveScope,
    choice: ResolveChoice,
) -> Result<Vec<RelPath>> {
    write_op(db, cancel, || {
        let nodes = if recursive {
            subtree(db, rel)?
        } else {
            vec![rel.clone()]
        };
        let mut done = Vec::new();
        for n in nodes {
            cancel.check()?;
            if resolve_one(db, &n, scope, choice)? {
                done.push(n);
            }
        }
        Ok(done)
    })
}

fn resolve_one(
    db: &WcDb,
    rel: &RelPath,
    scope: ResolveScope,
    choice: ResolveChoice,
) -> Result<bool> {
    let Some(record) = db.conflict(rel)? else {
        return Ok(false);
    };
    let mut skel = ConflictSkel::from_skel(&record.skel)?;
    let mut work: Vec<WorkItem> = Vec::new();
    let mut resolve_text = false;
    let mut resolve_props = false;
    let mut resolve_tree = false;
    let mut new_props_for_txn = None;

    if scope.text
        && let Some(text) = skel.text()
    {
        let source = match choice {
            ResolveChoice::Working => None,
            ResolveChoice::Base => Some(&text.original),
            ResolveChoice::MineFull => Some(&text.mine),
            ResolveChoice::TheirsFull => Some(&text.their),
        };
        if let Some(src) = source {
            let src = src
                .clone()
                .ok_or_else(|| invalid(format!("'{rel}' has no such conflict version")))?;
            work.push(WorkItem::FileTranslate {
                local: rel.as_str().to_owned(),
                src,
                dst: rel.as_str().to_owned(),
            });
        }
        for m in [&text.original, &text.mine, &text.their]
            .into_iter()
            .flatten()
        {
            work.push(WorkItem::FileRemove { relpath: m.clone() });
        }
        resolve_text = true;
    }
    if scope.props
        && let Some(prop) = skel.prop()
    {
        let new_props = match choice {
            ResolveChoice::Working => None,
            ResolveChoice::Base => Some(prop.old_props.clone().unwrap_or_default()),
            ResolveChoice::MineFull => Some(prop.mine_props.clone().unwrap_or_default()),
            ResolveChoice::TheirsFull => Some(prop.their_props.clone().unwrap_or_default()),
        };
        new_props_for_txn = new_props;
        work.extend(
            prop.markers
                .iter()
                .map(|m| WorkItem::FileRemove { relpath: m.clone() }),
        );
        resolve_props = true;
    }
    if scope.tree && skel.tree().is_some() {
        if choice != ResolveChoice::Working {
            return Err(invalid(format!(
                "tree conflict on '{rel}' can only be resolved with the working choice; use \
                 svn or TortoiseSVN for other tree conflict resolutions"
            )));
        }
        resolve_tree = true;
    }
    if !(resolve_text || resolve_props || resolve_tree) {
        return Ok(false);
    }
    let all_gone = skel.resolve(resolve_text, resolve_props.then_some(""), resolve_tree)?;
    // The chosen properties, the conflict record and the marker removals change together.
    db.with_txn(|t| {
        if let Some(p) = &new_props_for_txn {
            t.set_props(rel, p)?;
        }
        if all_gone {
            t.clear_conflict(rel)?;
        } else {
            t.write_conflict(
                rel,
                &ConflictRecord {
                    skel: skel.to_skel(),
                    texts: record.texts.clone(),
                },
            )?;
        }
        t.queue_work(&skels(&work))
    })?;
    Ok(true)
}

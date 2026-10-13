//! `svn changelist`: group files for a partial commit.

use std::path::PathBuf;

use svn_types::{CancelToken, Depth, ErrorCode, NodeKind, RelPath, Result, SvnError};
use svn_wc::WcDb;

use super::lock::write_op;
use super::paths::rel_of;
use super::state::{Schedule, node_info, subtree};

/// Puts the files under `targets` into changelist `name`, or removes them from any changelist
/// when `name` is `None`. Directories are never members: with `Depth::Empty` a directory target
/// is skipped, with `Depth::Infinity` its files are processed. Returns the affected files.
pub fn set_changelist(
    db: &WcDb,
    cancel: &CancelToken,
    targets: &[PathBuf],
    name: Option<&str>,
    depth: Depth,
) -> Result<Vec<RelPath>> {
    if let Some(n) = name
        && (n.is_empty() || n.contains('\n'))
    {
        return Err(SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!("invalid changelist name {n:?}"),
        ));
    }
    write_op(db, cancel, || {
        let mut changed = Vec::new();
        for t in targets {
            cancel.check()?;
            let rel = rel_of(db, t)?;
            let info = node_info(db, &rel)?;
            let candidates = if info.node.kind == NodeKind::Dir {
                if matches!(depth, Depth::Infinity | Depth::Unknown) {
                    subtree(db, &rel)?
                } else {
                    Vec::new()
                }
            } else {
                vec![rel]
            };
            for c in candidates {
                let ci = node_info(db, &c)?;
                if ci.node.kind != NodeKind::File || ci.schedule == Schedule::Deleted {
                    continue;
                }
                db.set_changelist(&c, name)?;
                changed.push(c);
            }
        }
        Ok(changed)
    })
}

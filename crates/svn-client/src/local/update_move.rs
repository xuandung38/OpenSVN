//! Resolving the tree conflict "local moved away, incoming edit" an update or switch leaves on
//! the source of a local move, like svn 1.15:
//!
//! - `mine-conflict` ([`update_destination`]): the changes the update brought to the source
//!   (its BASE tree went from the revision the destination was copied from to the new one) are
//!   carried to the destination, as svn's `svn_wc__db_update_moved_away_conflict_victim` does:
//!   texts merged into local edits (conflict markers `<file>.tmp`, `.2.tmp`, `.3.tmp`, hunks
//!   labelled `.working`, `.old`, `.new`), properties merged, nodes added and deleted. Where a
//!   change cannot be applied without touching something local (a node deleted, replaced or
//!   added locally, an unversioned item in the way, local changes on a node the server
//!   deleted), the destination gets a tree conflict instead and the local state is kept: a
//!   node with local changes the server deleted stays as a copy of its own. Then the
//!   destination layer becomes a copy of the source's new BASE (`replace_moved_layer`).
//! - `working` ([`break_move`]): the destination stays as it is and the move becomes a copy
//!   and a deletion.
//!
//! Every row change, conflict and queued work item of a resolution is written in one wc.db
//! transaction; the queue then makes the disk match. Local text never goes away: a modified
//! file is merged (its old text kept in the `.3.tmp` marker on conflict), never overwritten.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};

use svn_diff::{ConflictLabels, DiffError, MergeOptions, diff_as_binary_by_mime, merge3};
use svn_types::{CancelToken, ErrorCode, NodeKind, PropChange, RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::conflicts::markers::{
    MarkerSource, TextMarkerLabels, create_prop_marker, create_text_markers,
};
use svn_wc::conflicts::{
    ConflictSkel, Operation, OperationInfo, Origin, PropConflict, TextConflict, TreeAction,
    TreeReason,
};
use svn_wc::db::{ConflictRecord, LayerRow, Node, Presence, Props};
use svn_wc::pristine::Sha1Digest;
use svn_wc::workqueue::WorkItem;

use crate::update::conflict_record::{self, NewConflict, Side};
use crate::update::merge_props;
use crate::wc::{inspect, skels};

/// Largest text merged in memory; bigger ones conflict without a merge, as in an update.
const MAX_MERGE_BYTES: u64 = 64 * 1024 * 1024;

/// Both ends of the local move a "moved away" tree conflict names.
#[derive(Debug, Clone)]
pub(super) struct MovedAway {
    /// Source op root (the conflict victim).
    pub src: RelPath,
    /// Destination op root.
    pub dst: RelPath,
}

fn corrupt(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::Corrupt, msg)
}

/// The move of a "local moved away, incoming edit" conflict an update or switch recorded.
pub(super) fn victim(skel: &ConflictSkel) -> Result<Option<MovedAway>> {
    let Some(tree) = skel.tree() else {
        return Ok(None);
    };
    let op = skel.operation.as_ref().map(|o| &o.operation);
    if tree.reason != TreeReason::MovedAway
        || tree.action != TreeAction::Edit
        || !matches!(op, Some(Operation::Update | Operation::Switch))
    {
        return Ok(None);
    }
    let (Some(src), Some(dst)) = (&tree.move_src_op_root, &tree.move_dst_op_root) else {
        return Ok(None);
    };
    Ok(Some(MovedAway {
        src: RelPath::new(src)?,
        dst: RelPath::new(dst)?,
    }))
}

fn depth_of(p: &RelPath) -> u32 {
    if p.is_root() {
        0
    } else {
        u32::try_from(p.as_str().split('/').count()).unwrap_or(u32::MAX)
    }
}

fn join(root: &RelPath, suffix: &str) -> Result<RelPath> {
    if suffix.is_empty() {
        Ok(root.clone())
    } else {
        RelPath::new(&format!("{root}{suffix}"))
    }
}

fn kind_of(word: &str) -> NodeKind {
    match word {
        "dir" => NodeKind::Dir,
        "symlink" => NodeKind::Symlink,
        _ => NodeKind::File,
    }
}

/// A node of a layer that exists (rows that only mark a node absent do not).
fn present(r: &LayerRow) -> bool {
    matches!(r.presence.as_str(), "normal" | "incomplete")
}

/// Property changes that turn `old` into `new`.
fn prop_changes(old: &Props, new: &Props) -> Vec<PropChange> {
    let mut out = Vec::new();
    for (k, v) in new {
        if !old.iter().any(|(ok, ov)| ok == k && ov == v) {
            out.push(PropChange {
                name: String::from_utf8_lossy(k).into_owned(),
                value: Some(bytes::Bytes::copy_from_slice(v)),
            });
        }
    }
    for (k, _) in old {
        if !new.iter().any(|(nk, _)| nk == k) {
            out.push(PropChange {
                name: String::from_utf8_lossy(k).into_owned(),
                value: None,
            });
        }
    }
    out
}

fn pristine_relpath(sha1: &Sha1Digest) -> String {
    let hex = sha1.to_hex();
    format!(
        "{}/pristine/{}/{hex}.svn-base",
        svn_wc::db::ADM_DIR,
        &hex[..2]
    )
}

/// The revisions and repository of the conflict the resolution answers.
struct Versions {
    switch: bool,
    root: String,
    uuid: String,
    old_rev: u64,
    new_rev: u64,
}

impl Versions {
    fn of(op: &OperationInfo) -> Result<Self> {
        let loc = |o: &Origin| match o {
            Origin::Subversion(l) => Some(l.clone()),
            _ => None,
        };
        let (Some(left), Some(right)) = (loc(&op.left), loc(&op.right)) else {
            return Err(corrupt(
                "the moved-away conflict has no repository versions",
            ));
        };
        Ok(Self {
            switch: op.operation == Operation::Switch,
            root: left.repos_root_url.clone(),
            uuid: left.repos_uuid.clone().unwrap_or_default(),
            old_rev: u64::try_from(left.peg_rev).unwrap_or(0),
            new_rev: u64::try_from(right.peg_rev).unwrap_or(0),
        })
    }

    /// `(update (left right))` of one destination node: its repository path at the old and at
    /// the new revision, `none` on the side where it does not exist.
    fn info(&self, repos: &str, old: Option<NodeKind>, new: Option<NodeKind>) -> OperationInfo {
        let side = |rev, kind: Option<NodeKind>| Side {
            repos_relpath: repos.to_owned(),
            rev,
            kind: kind.unwrap_or(NodeKind::None),
        };
        conflict_record::operation_info(
            self.switch,
            &self.root,
            &self.uuid,
            Some(&side(self.old_rev, old)),
            Some(&side(self.new_rev, new)),
        )
    }
}

/// What the resolution changes, gathered before its transaction.
#[derive(Default)]
struct Plan {
    /// Destination nodes kept as a copy of their own (local changes the server deleted).
    keep: Vec<RelPath>,
    /// New ACTUAL properties (`None`: same as the new pristine ones).
    actual: Vec<(RelPath, Option<Props>)>,
    /// Conflicts to record, per node.
    conflicts: BTreeMap<RelPath, (OperationInfo, Vec<NewConflict>)>,
    /// Work items, in order.
    work: Vec<WorkItem>,
    /// Merge results the queue installs; removed again if the transaction fails.
    temps: Vec<tempfile::TempPath>,
}

impl Plan {
    fn conflict(&mut self, path: &RelPath, info: OperationInfo, c: NewConflict) {
        self.conflicts
            .entry(path.clone())
            .or_insert_with(|| (info, Vec::new()))
            .1
            .push(c);
    }
}

/// The deepest local layer above the destination layer `layer` at `path` or a folder above
/// it below `dst`: its op root and the op root's top row. Changes there are shadowed.
fn shadow(db: &WcDb, dst: &RelPath, layer: u32, path: &RelPath) -> Result<Option<(RelPath, Node)>> {
    let below = path
        .as_str()
        .get(dst.as_str().len()..)
        .unwrap_or("")
        .trim_start_matches('/');
    let mut at = dst.clone();
    for c in below.split('/').filter(|c| !c.is_empty()) {
        at = at.child(c)?;
        if let Some(n) = db.read_node(&at)?.filter(|n| n.op_depth > layer) {
            let root: Vec<&str> = at
                .as_str()
                .split('/')
                .take(usize::try_from(n.op_depth).unwrap_or(usize::MAX))
                .collect();
            let root = RelPath::new(&root.join("/"))?;
            let row = db.read_node(&root)?.unwrap_or(n);
            return Ok(Some((root, row)));
        }
    }
    Ok(None)
}

/// Local changes in the destination subtree at `path`: a modified text, modified properties,
/// or a local layer above the destination's.
fn locally_changed(
    db: &WcDb,
    old: &BTreeMap<String, LayerRow>,
    dst: &RelPath,
    suffix: &str,
    layer: u32,
) -> Result<bool> {
    for (s, row) in old
        .range(suffix.to_owned()..)
        .take_while(|(s, _)| *s == suffix || s.starts_with(&format!("{suffix}/")))
    {
        let p = join(dst, s)?;
        let Some(node) = db.read_node(&p)? else {
            continue;
        };
        if node.op_depth > layer {
            return Ok(true);
        }
        if !present(row) {
            continue;
        }
        let mut actual = db.actual_props(&p)?;
        actual.sort();
        if actual != row.props {
            return Ok(true);
        }
        if kind_of(&row.kind) != NodeKind::Dir && inspect::text_modified(db, &p, &node)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn disk_kind(db: &WcDb, p: &RelPath) -> Result<Option<NodeKind>> {
    let path = crate::wc::abs_path(db, p)?;
    Ok(match fs::symlink_metadata(&path) {
        Ok(m) if m.is_dir() => Some(NodeKind::Dir),
        Ok(m) if m.file_type().is_symlink() => Some(NodeKind::Symlink),
        Ok(_) => Some(NodeKind::File),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(SvnError::from_io(format!("cannot stat '{p}'"), &e)),
    })
}

fn read_text(db: &WcDb, sha: &Sha1Digest) -> Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    db.pristine_store()
        .open_text(sha)?
        .take(MAX_MERGE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| SvnError::from_io("cannot read a pristine text", &e))?;
    Ok((buf.len() as u64 <= MAX_MERGE_BYTES).then_some(buf))
}

fn write_tmp(db: &WcDb, bytes: &[u8]) -> Result<tempfile::TempPath> {
    let dir = db.wcroot().join(svn_wc::db::ADM_DIR).join("tmp");
    fs::create_dir_all(&dir)
        .map_err(|e| SvnError::from_io(format!("cannot create '{}'", dir.display()), &e))?;
    let mut tmp = tempfile::Builder::new()
        .prefix("merge-")
        .tempfile_in(&dir)
        .map_err(|e| SvnError::from_io("cannot create merge temp file", &e))?;
    tmp.write_all(bytes)
        .map_err(|e| SvnError::from_io("cannot write merge result", &e))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| SvnError::from_io("cannot sync merge result", &e))?;
    Ok(tmp.into_temp_path())
}

/// Merges the text change `old` -> `new` into the modified working file `p` (svn's
/// `svn_wc__internal_merge` without labels, as `update_working_file` calls it).
fn merge_text(
    db: &WcDb,
    cancel: &CancelToken,
    p: &RelPath,
    old: &Sha1Digest,
    new: &Sha1Digest,
    actual: &Props,
    plan: &mut Plan,
) -> Result<Option<TextConflict>> {
    let rel = p.as_str();
    let make_markers = || {
        create_text_markers(
            db.wcroot(),
            rel,
            &TextMarkerLabels {
                original: ".tmp".into(),
                mine: ".tmp".into(),
                their: ".tmp".into(),
            },
            &MarkerSource::InWc(pristine_relpath(old)),
            &MarkerSource::InWc(pristine_relpath(new)),
            &inspect::translate_info(actual),
        )
    };
    let mime =
        inspect::prop_value(actual, "svn:mime-type").and_then(|v| std::str::from_utf8(v).ok());
    let texts = if diff_as_binary_by_mime(mime) {
        None
    } else {
        match (read_text(db, old)?, read_text(db, new)?) {
            (Some(b), Some(t)) => Some((b, t)),
            _ => None,
        }
    };
    let Some((base, theirs)) = texts else {
        // Not merged: the working file stays, the markers hold the two versions.
        let m = make_markers()?;
        plan.work.extend(m.work_items);
        return Ok(Some(m.conflict));
    };
    let mine = inspect::working_repo_normal(db, p)?;
    let opts = MergeOptions {
        labels: ConflictLabels {
            mine: ".working".into(),
            base: ".old".into(),
            theirs: ".new".into(),
        },
        ..MergeOptions::default()
    };
    let result = match merge3(&base, &mine, &theirs, &opts, cancel) {
        Ok(r) => r,
        Err(DiffError::Cancelled) => {
            return Err(SvnError::new(ErrorCode::Cancelled, "merge cancelled"));
        }
        Err(DiffError::TooComplex) => {
            let m = make_markers()?;
            plan.work.extend(m.work_items);
            return Ok(Some(m.conflict));
        }
    };
    let conflict = if result.conflicts.is_empty() {
        None
    } else {
        let m = make_markers()?;
        plan.work.extend(m.work_items);
        Some(m.conflict)
    };
    let tmp = write_tmp(db, &result.text)?;
    let name = tmp
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| corrupt("merge temp file has no name"))?;
    let src = format!("{}/tmp/{name}", svn_wc::db::ADM_DIR);
    plan.work.push(WorkItem::FileInstall {
        relpath: rel.to_owned(),
        use_commit_times: false,
        record_fileinfo: false,
        source: Some(src.clone()),
    });
    plan.work.push(WorkItem::FileRemove { relpath: src });
    plan.temps.push(tmp);
    Ok(conflict)
}

/// An edit of the destination node `p` (present on both sides with the same kind).
#[allow(clippy::too_many_arguments)]
fn edit(
    db: &WcDb,
    cancel: &CancelToken,
    v: &Versions,
    p: &RelPath,
    repos: &str,
    old: &LayerRow,
    new: &LayerRow,
    plan: &mut Plan,
) -> Result<()> {
    let kind = kind_of(&new.kind);
    let text_changed = kind != NodeKind::Dir && old.checksum != new.checksum;
    if !text_changed && old.props == new.props {
        return Ok(());
    }
    let node = db
        .read_node(p)?
        .ok_or_else(|| corrupt(format!("'{p}' has no row in the move destination")))?;
    let mut actual = db.actual_props(p)?;
    actual.sort();
    let pm = merge_props(&old.props, &prop_changes(&old.props, &new.props), &actual);
    let info = v.info(repos, Some(kind_of(&old.kind)), Some(kind));
    if kind != NodeKind::Dir {
        let on_disk = disk_kind(db, p)?.is_some();
        let modified = on_disk && inspect::text_modified(db, p, &node)?;
        let translation = ["svn:eol-style", "svn:keywords", "svn:special"]
            .iter()
            .any(|n| inspect::prop_value(&old.props, n) != inspect::prop_value(&new.props, n));
        let flags = ["svn:executable", "svn:needs-lock"]
            .iter()
            .any(|n| inspect::prop_value(&old.props, n) != inspect::prop_value(&new.props, n));
        if on_disk && modified && text_changed {
            let parse = |c: &Option<String>| {
                c.as_deref()
                    .and_then(Sha1Digest::from_svn_string)
                    .ok_or_else(|| corrupt(format!("'{p}' has no pristine checksum")))
            };
            let (o, n) = (parse(&old.checksum)?, parse(&new.checksum)?);
            if let Some(t) = merge_text(db, cancel, p, &o, &n, &actual, plan)? {
                plan.conflict(p, info.clone(), NewConflict::Text(t));
            }
        } else if on_disk && !modified && (text_changed || translation) {
            plan.work.push(WorkItem::FileInstall {
                relpath: p.as_str().to_owned(),
                use_commit_times: false,
                record_fileinfo: true,
                source: None,
            });
        } else if on_disk && flags {
            plan.work.push(WorkItem::SyncFileFlags {
                relpath: p.as_str().to_owned(),
            });
        }
    }
    let overlay = (pm.new_actual != pm.new_pristine).then(|| pm.new_actual.clone());
    plan.actual.push((p.clone(), overlay));
    if !pm.conflicted.is_empty() {
        let marker = create_prop_marker(db.wcroot(), p.as_str(), kind == NodeKind::Dir)?;
        plan.work.push(marker.work_item);
        plan.conflict(
            p,
            info,
            NewConflict::Prop(PropConflict {
                markers: vec![marker.relpath],
                names: pm.conflicted.clone(),
                old_props: Some(old.props.clone()),
                mine_props: Some(actual.clone()),
                their_props: Some(new.props.clone()),
            }),
        );
    }
    Ok(())
}

/// `svn resolve --accept mine-conflict` of the "moved away, incoming edit" conflict on
/// `mv.src`: see the module documentation.
pub(super) fn update_destination(
    db: &WcDb,
    cancel: &CancelToken,
    mv: &MovedAway,
    record: &ConflictRecord,
) -> Result<()> {
    let mut skel = ConflictSkel::from_skel(&record.skel)?;
    let op = skel
        .operation
        .clone()
        .ok_or_else(|| corrupt("the moved-away conflict has no operation"))?;
    let v = Versions::of(&op)?;
    let layer = depth_of(&mv.dst);
    let (old, new, src_repos) = db.with_txn(|t| {
        let src_repos = t
            .read_base(&mv.src)?
            .and_then(|n| n.repos_relpath)
            .ok_or_else(|| corrupt(format!("'{}' has no BASE node", mv.src)))?;
        Ok((t.layer(&mv.dst, layer)?, t.layer(&mv.src, 0)?, src_repos))
    })?;
    let mut suffixes: Vec<&String> = old.keys().chain(new.keys()).collect();
    suffixes.sort();
    suffixes.dedup();
    let mut plan = Plan::default();
    // Roots below which the destination was handled as a whole (a tree conflict, a removal,
    // a node kept as a copy).
    let mut done: Vec<String> = Vec::new();
    for suffix in suffixes {
        cancel.check()?;
        if done
            .iter()
            .any(|d| suffix == d || suffix.starts_with(&format!("{d}/")))
        {
            continue;
        }
        let p = join(&mv.dst, suffix)?;
        let repos = format!("{src_repos}{suffix}");
        let o = old.get(suffix).filter(|r| present(r));
        let n = new.get(suffix).filter(|r| present(r));
        if o.is_none() && n.is_none() {
            continue;
        }
        // A local layer over the node: only the destination layer changes, the local state
        // stays, and its op root gets a tree conflict (once).
        if let Some((root, row)) = shadow(db, &mv.dst, layer, &p)? {
            let root_suffix = root.as_str()[mv.dst.as_str().len()..].to_owned();
            let ro = old.get(&root_suffix).filter(|r| present(r));
            let rn = new.get(&root_suffix).filter(|r| present(r));
            let reason = if row.presence == Presence::BaseDeleted {
                TreeReason::Deleted
            } else if ro.is_some() {
                TreeReason::Replaced
            } else {
                TreeReason::Added
            };
            let action = if root != p {
                TreeAction::Edit
            } else {
                match (ro, rn) {
                    (Some(_), Some(_)) => TreeAction::Edit,
                    (None, Some(_)) => TreeAction::Add,
                    _ => TreeAction::Delete,
                }
            };
            let info = v.info(
                &format!("{src_repos}{root_suffix}"),
                ro.map(|r| kind_of(&r.kind)),
                rn.map(|r| kind_of(&r.kind)),
            );
            plan.conflict(&root, info, NewConflict::Tree(reason, action));
            done.push(root_suffix);
            continue;
        }
        let replaced = matches!((o, n), (Some(o), Some(n)) if o.kind != n.kind);
        if let Some(o) = o.filter(|_| n.is_none() || replaced) {
            if locally_changed(db, &old, &mv.dst, suffix, layer)? {
                plan.keep.push(p.clone());
                plan.conflict(
                    &p,
                    v.info(&repos, Some(kind_of(&o.kind)), None),
                    NewConflict::Tree(TreeReason::Edited, TreeAction::Delete),
                );
                done.push(suffix.clone());
                continue;
            }
            // Deepest first: files, then the folders once they are empty.
            let mut gone: Vec<(&String, &LayerRow)> = old
                .range(suffix.clone()..)
                .take_while(|(s, _)| *s == suffix || s.starts_with(&format!("{suffix}/")))
                .filter(|(_, r)| present(r))
                .collect();
            gone.reverse();
            for (s, r) in gone {
                let relpath = join(&mv.dst, s)?.as_str().to_owned();
                plan.work.push(if kind_of(&r.kind) == NodeKind::Dir {
                    WorkItem::DirRemove {
                        relpath,
                        recursive: false,
                    }
                } else {
                    WorkItem::FileRemove { relpath }
                });
            }
            if !replaced {
                done.push(suffix.clone());
                continue;
            }
        }
        match (o, n) {
            (Some(o), Some(n)) if !replaced => edit(db, cancel, &v, &p, &repos, o, n, &mut plan)?,
            (_, Some(n)) => {
                if disk_kind(db, &p)?.is_some() && !replaced {
                    plan.conflict(
                        &p,
                        v.info(&repos, None, Some(kind_of(&n.kind))),
                        NewConflict::Tree(TreeReason::Unversioned, TreeAction::Add),
                    );
                    done.push(suffix.clone());
                    continue;
                }
                plan.work.push(if kind_of(&n.kind) == NodeKind::Dir {
                    WorkItem::DirInstall {
                        relpath: p.as_str().to_owned(),
                    }
                } else {
                    WorkItem::FileInstall {
                        relpath: p.as_str().to_owned(),
                        use_commit_times: false,
                        record_fileinfo: true,
                        source: None,
                    }
                });
            }
            _ => {}
        }
    }
    let all_gone = skel.resolve(false, None, true)?;
    let Plan {
        keep,
        actual,
        conflicts,
        work,
        temps,
    } = plan;
    db.with_txn(|t| {
        for k in &keep {
            t.keep_moved_as_working(k, layer)?;
        }
        t.replace_moved_layer(&mv.src, &mv.dst)?;
        for (p, props) in &actual {
            t.set_actual_props(p, props.as_ref())?;
        }
        for (p, (info, list)) in conflicts {
            conflict_record::record(t, &p, &info, list)?;
        }
        if all_gone {
            t.clear_conflict(&mv.src)?;
        } else {
            t.write_conflict(
                &mv.src,
                &ConflictRecord {
                    skel: skel.to_skel(),
                    texts: record.texts.clone(),
                },
            )?;
        }
        t.queue_work(&skels(&work))
    })?;
    // The queue owns the merge results now.
    for tmp in temps {
        let _ = tmp.keep();
    }
    crate::wc::run_work_queue(db, cancel)?;
    Ok(())
}

/// `svn resolve --accept working` of the "moved away" conflict on `mv.src`: the destination
/// stays as it is and the move is broken (svn's `svn_wc__db_resolve_break_moved_away`).
pub(super) fn break_move(db: &WcDb, mv: &MovedAway, record: &ConflictRecord) -> Result<()> {
    let mut skel = ConflictSkel::from_skel(&record.skel)?;
    let all_gone = skel.resolve(false, None, true)?;
    db.with_txn(|t| {
        t.break_move(&mv.src, &mv.dst)?;
        if all_gone {
            t.clear_conflict(&mv.src)
        } else {
            t.write_conflict(
                &mv.src,
                &ConflictRecord {
                    skel: skel.to_skel(),
                    texts: record.texts.clone(),
                },
            )
        }
    })
}

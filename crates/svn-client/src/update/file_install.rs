//! Installing one updated file: pristine, property merge, local-modification merge, conflict
//! markers, and the single transaction that makes the new state durable.

use std::fs::{self, File};
use std::io::Read;
use std::sync::{Arc, PoisonError};

use svn_diff::{ConflictLabels, DiffError, MergeOptions, diff_as_binary_by_mime, merge3};
use svn_types::{Checksum, ErrorCode, NodeKind, RelPath, Result, SvnError};
use svn_wc::conflicts::markers::{
    MarkerSource, TextMarkerLabels, create_prop_marker, create_text_markers, preserved_extension,
};
use svn_wc::conflicts::{PropConflict, TextConflict, TreeReason};
use svn_wc::db::{BaseNode, Node, Presence, Props, WcDb};
use svn_wc::pristine::{Md5Digest, PristineRecord, Sha1Digest};
use svn_wc::skel::Skel;
use svn_wc::workqueue::WorkItem;

use super::conflict_record::{self, NewConflict};
use super::editor::{UpdateEditor, failpoint};
use super::props::merge_props;
use super::state::{FileState, tmp_dir};
use crate::ctx::UpdateAction;
use crate::wc::inspect;

/// Files above this size are not merged in memory; they become a conflict instead (svn would
/// merge them). Known limitation: such files need manual resolution after an update.
const MAX_MERGE_BYTES: u64 = 64 * 1024 * 1024;

fn corrupt(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::Corrupt, msg)
}

fn prop_changed(old: &Props, new: &Props, name: &str) -> bool {
    inspect::prop_value(old, name) != inspect::prop_value(new, name)
}

/// Working copy relative path of a stored pristine text.
fn pristine_relpath(sha1: &Sha1Digest) -> String {
    let hex = sha1.to_hex();
    format!(
        "{}/pristine/{}/{hex}.svn-base",
        svn_wc::db::ADM_DIR,
        &hex[..2]
    )
}

fn read_limited(mut r: impl Read, limit: u64) -> Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    let n = (&mut r)
        .take(limit + 1)
        .read_to_end(&mut buf)
        .map_err(|e| SvnError::from_io("cannot read file", &e))?;
    Ok(if n as u64 > limit { None } else { Some(buf) })
}

/// How the incoming text meets the local one.
enum TextMerge {
    /// Local changes merged cleanly into `tmp`.
    Merged(tempfile::TempPath),
    /// Conflict; `tmp` holds the file with markers when one could be produced.
    Conflict {
        markers: TextConflict,
        marker_items: Vec<WorkItem>,
        merged: Option<tempfile::TempPath>,
    },
}

impl UpdateEditor {
    pub(super) fn finish_file(
        &mut self,
        f: FileState,
        text_checksum: Option<Checksum>,
    ) -> Result<()> {
        let mut new_text = f.text.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let (Some(cs), Some(nt)) = (&text_checksum, &new_text) {
            let ok = match cs {
                Checksum::Md5(m) => *m == nt.md5,
                Checksum::Sha1(s) => *s == nt.sha1,
            };
            if !ok {
                return Err(
                    corrupt(format!("checksum mismatch for the new text of '{}'", f.wc))
                        .with_svn_code(200_014),
                );
            }
        }
        if f.skipped {
            return Ok(());
        }
        let db = Arc::clone(&self.s.db);
        let wc = f.wc.clone();
        let rev = self.rev()?;
        let old = f.base.clone();
        let old_pristine: Props = old
            .as_ref()
            .and_then(|b| b.properties.clone())
            .unwrap_or_default();
        // A node deleted or replaced locally, or lying below such a directory, is shadowed: only
        // BASE moves on, nothing is merged into ACTUAL and the working layer is left alone (svn
        // records just the tree conflict).
        let shadowed = f.shadow.is_some()
            || f.tree_conflict
                .as_ref()
                .is_some_and(|t| matches!(t.reason, TreeReason::Deleted | TreeReason::Replaced));
        let actual: Props = if shadowed {
            old_pristine.clone()
        } else if old.is_some() {
            db.actual_props(&wc)?
        } else {
            Vec::new()
        };
        let pm = merge_props(&old_pristine, &f.prop_changes, &actual);

        // 1. New pristine text: the delta result was hashed while it was written, so the file
        // moves into the store as it is. Its row is recorded with the node (see `super::batch`);
        // without a per-file sync, the batch syncs the filesystem before it commits.
        let mut new_pristine: Option<PristineRecord> = None;
        let (new_checksum, new_sha) = match new_text.take() {
            Some(nt) => {
                let record = PristineRecord {
                    sha1: Sha1Digest(nt.sha1),
                    md5: Md5Digest(nt.md5),
                    size: nt.size,
                    hydrated: true,
                };
                let synced = !svn_wc::durable::BATCHED_SYNC;
                let stored = db.pristine_store().place(nt.path, &record, synced)?;
                if !stored {
                    self.batch.placed_text(record.size, synced);
                }
                let sha = record.sha1;
                new_pristine = Some(record);
                (Some(sha.to_svn_string()), Some(sha))
            }
            None => {
                let cs = old.as_ref().and_then(|b| b.checksum.clone());
                (
                    cs.clone(),
                    cs.as_deref().and_then(Sha1Digest::from_svn_string),
                )
            }
        };
        let old_checksum = old.as_ref().and_then(|b| b.checksum.clone());
        let text_changed = new_pristine.is_some() && new_checksum != old_checksum;

        // 2. Decide what happens to the working file.
        let rel_str = wc.as_str().to_owned();
        let mut work: Vec<Skel> = Vec::new();
        let mut action = if f.added {
            UpdateAction::Add
        } else {
            UpdateAction::Update
        };
        let mut conflicts: Vec<NewConflict> = Vec::new();
        let mut keep_fileinfo = true;
        // Merge results the queued work installs; deleted again unless the transaction commits.
        let mut merge_tmp: Vec<tempfile::TempPath> = Vec::new();
        // Local delete or replace versus edit, or an unversioned file in the way of an add: BASE
        // moves forward and the working tree stays as it is.
        let keep_working_tree = shadowed
            || f.tree_conflict
                .as_ref()
                .is_some_and(|t| t.reason == TreeReason::Unversioned);
        let obstructed = f
            .tree_conflict
            .as_ref()
            .is_some_and(|t| t.reason == TreeReason::Unversioned);
        let mut new_props_changed_translation = false;
        for name in ["svn:eol-style", "svn:keywords", "svn:special"] {
            new_props_changed_translation |= prop_changed(&old_pristine, &pm.new_pristine, name);
        }
        let flags_changed = ["svn:executable", "svn:needs-lock"]
            .iter()
            .any(|n| prop_changed(&old_pristine, &pm.new_pristine, n));
        let disk_exists = fs::symlink_metadata(crate::wc::abs_path(&db, &wc)?).is_ok();

        if keep_working_tree {
            action = UpdateAction::TreeConflicted;
        } else if f.added {
            if f.adopt_existing && disk_exists {
                action = UpdateAction::Existed;
                keep_fileinfo = false;
            } else {
                work.push(
                    WorkItem::FileInstall {
                        relpath: rel_str.clone(),
                        use_commit_times: self.s.ctx.config.use_commit_times,
                        record_fileinfo: true,
                        source: None,
                    }
                    .to_skel(),
                );
            }
        } else if text_changed {
            let base_node = old
                .as_ref()
                .ok_or_else(|| corrupt("open file without base"))?;
            let local_mod = disk_exists && inspect::text_modified(&db, &wc, base_node)?;
            if !local_mod {
                work.push(
                    WorkItem::FileInstall {
                        relpath: rel_str.clone(),
                        use_commit_times: self.s.ctx.config.use_commit_times,
                        record_fileinfo: true,
                        source: None,
                    }
                    .to_skel(),
                );
            } else {
                keep_fileinfo = false;
                let merged = self.merge_local(
                    &db,
                    &wc,
                    base_node,
                    new_sha.ok_or_else(|| corrupt("missing new checksum"))?,
                    &pm.new_pristine,
                    &actual,
                    rev,
                )?;
                match merged {
                    TextMerge::Merged(tmp) => {
                        action = UpdateAction::Merged;
                        work.extend(install_merged(&rel_str, &tmp, false)?);
                        merge_tmp.push(tmp);
                    }
                    TextMerge::Conflict {
                        markers,
                        marker_items,
                        merged,
                    } => {
                        action = UpdateAction::Conflicted;
                        work.extend(marker_items.iter().map(WorkItem::to_skel));
                        if let Some(tmp) = merged {
                            work.extend(install_merged(&rel_str, &tmp, true)?);
                            merge_tmp.push(tmp);
                        }
                        conflicts.push(NewConflict::Text(markers));
                    }
                }
            }
        } else if disk_exists {
            let local_mod = match old.as_ref() {
                Some(b) => inspect::text_modified(&db, &wc, b)?,
                None => false,
            };
            if new_props_changed_translation && !local_mod {
                work.push(
                    WorkItem::FileInstall {
                        relpath: rel_str.clone(),
                        use_commit_times: self.s.ctx.config.use_commit_times,
                        record_fileinfo: true,
                        source: None,
                    }
                    .to_skel(),
                );
            } else if flags_changed {
                work.push(
                    WorkItem::SyncFileFlags {
                        relpath: rel_str.clone(),
                    }
                    .to_skel(),
                );
            }
        }
        if action == UpdateAction::Update
            && !f.added
            && !text_changed
            && !pm.applied
            && work.is_empty()
        {
            // Nothing visible changed (a pure revision bump); keep the event stream quiet.
        }

        // 3. Property conflict.
        if !pm.conflicted.is_empty() {
            let marker = create_prop_marker(db.wcroot(), &rel_str, false)?;
            work.push(marker.work_item.to_skel());
            conflicts.push(NewConflict::Prop(PropConflict {
                markers: vec![marker.relpath],
                names: pm.conflicted.clone(),
                old_props: Some(old_pristine.clone()),
                mine_props: Some(actual.clone()),
                their_props: Some(pm.new_pristine.clone()),
            }));
            action = UpdateAction::Conflicted;
        }
        if let Some(tc) = &f.tree_conflict {
            conflicts.push(NewConflict::Tree(tc.reason.clone(), tc.action.clone()));
        }

        // 4. One transaction.
        let (keep_size, keep_mtime) =
            if keep_fileinfo && !keep_working_tree && !text_changed && work.is_empty() {
                old.as_ref()
                    .map_or((None, None), |b| (b.translated_size, b.last_mod_time))
            } else {
                (None, None)
            };
        let node = BaseNode {
            kind: NodeKind::File,
            presence: Presence::Normal,
            repos_root_url: self.s.repos_root.clone(),
            repos_uuid: self.s.uuid.clone(),
            repos_relpath: f.new_repos.clone(),
            revision: rev,
            changed_rev: f
                .entry
                .rev
                .or_else(|| old.as_ref().and_then(|b| b.changed_rev)),
            changed_date: f
                .entry
                .date
                .or_else(|| old.as_ref().and_then(|b| b.changed_date)),
            changed_author: f
                .entry
                .author
                .clone()
                .or_else(|| old.as_ref().and_then(|b| b.changed_author.clone())),
            depth: None,
            properties: Some(pm.new_pristine.clone()),
            checksum: new_checksum,
            symlink_target: None,
            translated_size: keep_size,
            last_mod_time: keep_mtime,
            inherited_props: None,
            dav_cache: None,
        };
        let overlay = if pm.new_actual == pm.new_pristine {
            None
        } else {
            Some(pm.new_actual.clone())
        };
        let (old_side, new_side) = self.side(old.as_ref(), &f.new_repos, NodeKind::File);
        let info = conflict_record::operation_info(
            self.s.switch_relpath.is_some(),
            &self.s.repos_root,
            &self.s.uuid,
            old_side.as_ref(),
            new_side.as_ref(),
        );
        let file_external = self
            .s
            .file_external
            .clone()
            .filter(|_| wc == self.target_wc);
        failpoint!("update.after_merge3_before_txn");
        // The queue owns the merge results once the batch commits (it removes them after
        // installing); until then they are removed again on failure.
        for tmp in merge_tmp {
            self.batch.keep_on_commit(tmp);
        }
        let path = wc.clone();
        let shadow = f.shadow;
        let had_base = old.is_some();
        self.defer(Box::new(move |t| {
            if let Some(record) = &new_pristine {
                t.record_pristine(record)?;
            }
            t.base_add(&path, &node)?;
            if let Some(ext) = &file_external {
                t.mark_file_external(&path)?;
                t.external_add(&path, ext)?;
            }
            if let Some(op_depth) = shadow {
                t.mark_base_deleted_at(&path, NodeKind::File, op_depth)?;
            } else if obstructed {
                t.mark_base_deleted(&path, NodeKind::File)?;
            }
            if !shadowed && (had_base || overlay.is_some()) {
                t.set_actual_props(&path, overlay.as_ref())?;
            }
            if !conflicts.is_empty() {
                conflict_record::record(t, &path, &info, conflicts)?;
            }
            t.queue_work(&work)
        }))?;
        if f.shadow.is_some() {
            // Shadowed by an obstruction: only BASE changed, nothing to report.
            return Ok(());
        }
        self.emit(&wc, NodeKind::File, action);
        if matches!(
            action,
            UpdateAction::Conflicted | UpdateAction::TreeConflicted
        ) {
            self.skipped.push(wc);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn merge_local(
        &self,
        db: &WcDb,
        wc: &RelPath,
        base: &Node,
        new_sha: Sha1Digest,
        new_props: &Props,
        actual: &Props,
        new_rev: u64,
    ) -> Result<TextMerge> {
        let rel_str = wc.as_str();
        let old_rev = base.revision.unwrap_or(0);
        let ext = preserved_extension(rel_str, &self.s.ctx.config.preserved_conflict_exts);
        let labels = TextMarkerLabels::for_update(
            i64::try_from(old_rev).ok(),
            i64::try_from(new_rev).unwrap_or(0),
            ext,
        );
        let old_sha = base
            .checksum
            .as_deref()
            .and_then(Sha1Digest::from_svn_string)
            .ok_or_else(|| corrupt("base file has no checksum"))?;
        let mime = inspect::prop_value(new_props, "svn:mime-type")
            .or_else(|| inspect::prop_value(actual, "svn:mime-type"))
            .and_then(|v| std::str::from_utf8(v).ok());
        let store = db.pristine_store();
        // The new text sits in the store already; its row is still pending (see `super::batch`),
        // so it is read by path.
        let new_text_path = store.path_of(&new_sha);
        let sources_ok = {
            let old_size = store
                .path_of(&old_sha)
                .metadata()
                .map(|m| m.len())
                .unwrap_or(0);
            let new_size = fs::metadata(&new_text_path).map(|m| m.len()).unwrap_or(0);
            let mine_size = fs::metadata(crate::wc::abs_path(db, wc)?)
                .map(|m| m.len())
                .unwrap_or(0);
            old_size.max(new_size).max(mine_size) <= MAX_MERGE_BYTES
        };
        let treat_binary = diff_as_binary_by_mime(mime) || !sources_ok;
        let old_path = pristine_relpath(&old_sha);
        let new_path = pristine_relpath(&new_sha);
        let make_markers = || -> Result<_> {
            let info = inspect::translate_info(actual);
            create_text_markers(
                db.wcroot(),
                rel_str,
                &labels,
                &MarkerSource::InWc(old_path.clone()),
                &MarkerSource::InWc(new_path.clone()),
                &info,
            )
        };
        if treat_binary {
            let (markers, marker_items) = without_mine_marker(db, make_markers()?);
            return Ok(TextMerge::Conflict {
                markers,
                marker_items,
                merged: None,
            });
        }
        let base_bytes = read_limited(store.open_text(&old_sha)?, MAX_MERGE_BYTES)?
            .ok_or_else(|| corrupt("base text too large to merge"))?;
        let theirs = read_limited(
            File::open(&new_text_path)
                .map_err(|e| SvnError::from_io("cannot open the new text", &e))?,
            MAX_MERGE_BYTES,
        )?
        .ok_or_else(|| corrupt("new text too large to merge"))?;
        let mine = inspect::working_repo_normal(db, wc)?;
        let opts = MergeOptions {
            labels: ConflictLabels {
                mine: labels.mine.clone(),
                base: labels.original.clone(),
                theirs: labels.their.clone(),
            },
            ..MergeOptions::default()
        };
        let result = match merge3(&base_bytes, &mine, &theirs, &opts, &self.s.cancel) {
            Ok(r) => r,
            Err(DiffError::Cancelled) => {
                return Err(SvnError::new(ErrorCode::Cancelled, "merge cancelled"));
            }
            Err(DiffError::TooComplex) => {
                let (markers, marker_items) = without_mine_marker(db, make_markers()?);
                return Ok(TextMerge::Conflict {
                    markers,
                    marker_items,
                    merged: None,
                });
            }
        };
        failpoint!("update.after_merge3_before_markers");
        let markers = if result.conflicts.is_empty() {
            None
        } else {
            Some(make_markers()?)
        };
        let tmp = write_tmp(db, &result.text)?;
        Ok(match markers {
            None => TextMerge::Merged(tmp),
            Some(m) => TextMerge::Conflict {
                markers: m.conflict,
                marker_items: m.work_items,
                merged: Some(tmp),
            },
        })
    }
}

/// Writes `bytes` to a synced temp file in the administrative tmp directory.
fn write_tmp(db: &WcDb, bytes: &[u8]) -> Result<tempfile::TempPath> {
    use std::io::Write;
    let dir = tmp_dir(db);
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

/// Work items that install the merged text and remove its temporary file afterwards.
fn install_merged(rel: &str, tmp: &tempfile::TempPath, _conflicted: bool) -> Result<Vec<Skel>> {
    let name = tmp
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| corrupt("merge temp file has no name"))?;
    let src = format!("{}/tmp/{name}", svn_wc::db::ADM_DIR);
    Ok(vec![
        WorkItem::FileInstall {
            relpath: rel.to_owned(),
            use_commit_times: false,
            record_fileinfo: false,
            source: Some(src.clone()),
        }
        .to_skel(),
        WorkItem::FileRemove { relpath: src }.to_skel(),
    ])
}

/// Markers for a conflict whose working file is left untouched: svn keeps the user's version in
/// place and records no `.mine` file for it, so that marker and its work items are dropped.
fn without_mine_marker(
    db: &WcDb,
    m: svn_wc::conflicts::markers::TextMarkers,
) -> (TextConflict, Vec<WorkItem>) {
    let mut conflict = m.conflict;
    let mine = conflict.mine.take();
    let mut staged: Option<String> = None;
    let mut items: Vec<WorkItem> = Vec::new();
    for item in m.work_items {
        match &item {
            WorkItem::FileTranslate { dst, src, .. } if Some(dst) == mine.as_ref() => {
                staged = Some(src.clone());
            }
            _ => items.push(item),
        }
    }
    items.retain(
        |i| !matches!(i, WorkItem::FileRemove { relpath } if Some(relpath) == staged.as_ref()),
    );
    // Both files only exist to feed the dropped items.
    for rel in mine.iter().chain(staged.iter()) {
        let _ = fs::remove_file(db.wcroot().join(rel));
    }
    (conflict, items)
}

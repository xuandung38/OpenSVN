//! Repository side of `status -u`: a status-mode drive whose editor only notes what changed.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use svn_ra_api::{RaSession, UpdateMode, UpdateRequest};
use svn_types::{
    CancelToken, Checksum, DeltaEditor, Depth, DirId, FileId, LockInfo, NodeKind, PropChange,
    RelPath, RemoteStatus, Result, Revnum, SvnError, WcStatus, WindowSink,
};
use svn_wc::db::WcDb;

use crate::update::build_report;
use crate::wc::{self, join_rel};

/// What the repository has for one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteChange {
    /// Kind of node on the server side (`None` for a deletion).
    pub kind: NodeKind,
    /// The path no longer exists in the repository.
    pub deleted: bool,
}

#[derive(Default)]
struct Collected {
    changes: BTreeMap<RelPath, RemoteChange>,
    revision: Option<Revnum>,
}

/// Editor that records out-of-date paths.
struct StatusEditor {
    anchor: RelPath,
    out: Arc<Mutex<Collected>>,
    dirs: std::collections::HashMap<u32, (RelPath, bool)>,
    files: std::collections::HashMap<u32, RelPath>,
    next: u32,
}

impl StatusEditor {
    fn note(&self, wc: RelPath, kind: NodeKind, deleted: bool) {
        self.out
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .changes
            .insert(wc, RemoteChange { kind, deleted });
    }

    fn dir_path(&self, id: DirId) -> Result<RelPath> {
        self.dirs
            .get(&id.0)
            .map(|d| d.0.clone())
            .ok_or_else(|| SvnError::new(svn_types::ErrorCode::Internal, "unknown directory"))
    }
}

struct Sink;
impl WindowSink for Sink {
    fn window(&mut self, _: Option<&svn_types::TxDeltaWindow>) -> Result<()> {
        Ok(())
    }
}

fn is_real_prop(name: &str) -> bool {
    !name.starts_with("svn:entry:") && !name.starts_with("svn:wc:") && !name.starts_with("svn:dav:")
}

impl DeltaEditor for StatusEditor {
    fn set_target_revision(&mut self, rev: Revnum) -> Result<()> {
        self.out
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .revision = Some(rev);
        Ok(())
    }
    fn open_root(&mut self, _: Option<Revnum>) -> Result<DirId> {
        self.next += 1;
        self.dirs.insert(self.next, (self.anchor.clone(), false));
        Ok(DirId(self.next))
    }
    fn delete_entry(&mut self, path: &RelPath, _: Option<Revnum>, _: DirId) -> Result<()> {
        self.note(join_rel(&self.anchor, path)?, NodeKind::None, true);
        Ok(())
    }
    fn add_directory(
        &mut self,
        path: &RelPath,
        _: DirId,
        _: Option<(&str, Revnum)>,
    ) -> Result<DirId> {
        let wc = join_rel(&self.anchor, path)?;
        self.note(wc.clone(), NodeKind::Dir, false);
        self.next += 1;
        self.dirs.insert(self.next, (wc, true));
        Ok(DirId(self.next))
    }
    fn open_directory(&mut self, path: &RelPath, _: DirId, _: Option<Revnum>) -> Result<DirId> {
        self.next += 1;
        self.dirs
            .insert(self.next, (join_rel(&self.anchor, path)?, false));
        Ok(DirId(self.next))
    }
    fn change_dir_prop(&mut self, dir: DirId, change: PropChange) -> Result<()> {
        if is_real_prop(&change.name) {
            let wc = self.dir_path(dir)?;
            self.note(wc, NodeKind::Dir, false);
        }
        Ok(())
    }
    fn close_directory(&mut self, dir: DirId) -> Result<()> {
        self.dirs.remove(&dir.0);
        Ok(())
    }
    fn absent_directory(&mut self, _: &RelPath, _: DirId) -> Result<()> {
        Ok(())
    }
    fn add_file(&mut self, path: &RelPath, _: DirId, _: Option<(&str, Revnum)>) -> Result<FileId> {
        let wc = join_rel(&self.anchor, path)?;
        self.note(wc.clone(), NodeKind::File, false);
        self.next += 1;
        self.files.insert(self.next, wc);
        Ok(FileId(self.next))
    }
    fn open_file(&mut self, path: &RelPath, _: DirId, _: Option<Revnum>) -> Result<FileId> {
        let wc = join_rel(&self.anchor, path)?;
        self.note(wc.clone(), NodeKind::File, false);
        self.next += 1;
        self.files.insert(self.next, wc);
        Ok(FileId(self.next))
    }
    fn apply_textdelta(&mut self, _: FileId, _: Option<Checksum>) -> Result<Box<dyn WindowSink>> {
        Ok(Box::new(Sink))
    }
    fn change_file_prop(&mut self, _: FileId, _: PropChange) -> Result<()> {
        Ok(())
    }
    fn close_file(&mut self, file: FileId, _: Option<Checksum>) -> Result<()> {
        self.files.remove(&file.0);
        Ok(())
    }
    fn absent_file(&mut self, _: &RelPath, _: DirId) -> Result<()> {
        Ok(())
    }
    fn close_edit(&mut self) -> Result<()> {
        Ok(())
    }
    fn abort_edit(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Everything `status -u` learns from the repository.
#[derive(Debug, Default)]
pub struct RemoteInfo {
    /// Changed paths (working copy relative) and what changed.
    pub changes: BTreeMap<RelPath, RemoteChange>,
    /// Revision the comparison was made against.
    pub revision: Option<Revnum>,
    /// Repository locks by working copy path.
    pub locks: BTreeMap<RelPath, LockInfo>,
}

/// Asks the server what changed below `rel` since the working copy revisions.
pub async fn fetch(
    session: &dyn RaSession,
    db: Arc<WcDb>,
    anchor: RelPath,
    target: RelPath,
    depth: Depth,
    repos_root: String,
    cancel: &CancelToken,
) -> Result<RemoteInfo> {
    let out = Arc::new(Mutex::new(Collected::default()));
    let editor = StatusEditor {
        anchor: anchor.clone(),
        out: Arc::clone(&out),
        dirs: Default::default(),
        files: Default::default(),
        next: 0,
    };
    let report = build_report(
        Arc::clone(&db),
        anchor.clone(),
        target.clone(),
        depth,
        false,
        repos_root,
    );
    session
        .do_update(
            UpdateRequest {
                revision: None,
                target: target.clone(),
                depth,
                mode: UpdateMode::Status,
                send_copyfrom: false,
                ignore_ancestry: false,
            },
            report,
            Box::new(editor),
            cancel,
        )
        .await?;
    let mut info = {
        let c = out.lock().unwrap_or_else(PoisonError::into_inner);
        RemoteInfo {
            changes: c.changes.clone(),
            revision: c.revision,
            locks: BTreeMap::new(),
        }
    };
    let locks = session.get_locks(&target, Depth::Infinity, cancel).await?;
    for l in locks {
        // Lock paths are repository paths below the session; map them back through the anchor.
        let text = l.path.as_str().trim_start_matches('/');
        if let Ok(rel) = RelPath::new(text)
            && let Ok(wc) = wc::join_rel(&anchor, &rel)
        {
            info.locks.insert(wc, l);
        }
    }
    Ok(info)
}

/// Adds the repository side to the local statuses and appends paths that only exist remotely.
pub fn merge(local: Vec<WcStatus>, info: &RemoteInfo) -> Vec<WcStatus> {
    let mut out = Vec::with_capacity(local.len());
    let mut seen = std::collections::BTreeSet::new();
    for mut s in local {
        let change = info.changes.get(&s.path);
        let lock = info.locks.get(&s.path).cloned();
        if change.is_some() || lock.is_some() {
            s.remote = Some(RemoteStatus {
                out_of_date: change.is_some(),
                remote_rev: change.and(info.revision),
                remote_lock: lock,
            });
        }
        seen.insert(s.path.clone());
        out.push(s);
    }
    for (path, change) in &info.changes {
        if seen.contains(path) || change.deleted {
            continue;
        }
        out.push(WcStatus {
            path: path.clone(),
            kind: change.kind,
            text: svn_types::NodeStatus::None,
            props: svn_types::PropStatus::None,
            tree_conflict: false,
            moved_from: None,
            moved_to: None,
            changelist: None,
            lock: None,
            lock_is_mine: false,
            needs_lock: false,
            binary: false,
            size: None,
            remote: Some(RemoteStatus {
                out_of_date: true,
                remote_rev: info.revision,
                remote_lock: info.locks.get(path).cloned(),
            }),
            switched: false,
            file_external: false,
            depth: Depth::Unknown,
            base_rev: None,
        });
    }
    out.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
    out
}

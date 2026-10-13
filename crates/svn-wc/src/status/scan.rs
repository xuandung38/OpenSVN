//! Parallel scan of the working copy directory tree.
//!
//! Only directories that wc.db knows are descended into (an unversioned or ignored directory is
//! reported as one item and not entered, like svn). Entries are `lstat`ed inside the worker
//! threads, which is where the time goes on a cold large working copy.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::UNIX_EPOCH;

use ignore::{WalkBuilder, WalkState};
use svn_types::{ErrorCode, RelPath, SvnError};

use crate::db::ADM_DIR;

/// Kind of a filesystem entry, from `lstat` (symlinks are not followed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FsKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// What the scan learned about one path.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FsEntry {
    pub kind: FsKind,
    pub size: u64,
    /// Modification time in microseconds since the epoch (APR time, truncated like APR).
    pub mtime_micros: i64,
}

impl FsEntry {
    pub(crate) fn from_metadata(md: &std::fs::Metadata) -> Self {
        let ft = md.file_type();
        let kind = if ft.is_symlink() {
            FsKind::Symlink
        } else if ft.is_dir() {
            FsKind::Dir
        } else if ft.is_file() {
            FsKind::File
        } else {
            FsKind::Other
        };
        let mtime_micros = md
            .modified()
            .ok()
            .map(|t| match t.duration_since(UNIX_EPOCH) {
                Ok(d) => i64::try_from(d.as_micros()).unwrap_or(i64::MAX),
                Err(e) => -i64::try_from(e.duration().as_micros()).unwrap_or(i64::MAX),
            })
            .unwrap_or(0);
        Self { kind, size: md.len(), mtime_micros }
    }
}

type Collected = Vec<(String, FsEntry)>;

/// Per-thread buffer that hands its entries to the shared sink when the worker finishes.
struct Batch {
    local: Collected,
    sink: Arc<Mutex<Collected>>,
}

impl Drop for Batch {
    fn drop(&mut self) {
        let mut sink = self.sink.lock().unwrap_or_else(PoisonError::into_inner);
        sink.append(&mut self.local);
    }
}

/// Walks `target` below `wc_root`, entering only directories listed in `descend` (relpaths).
/// Returns every entry found, including the target itself, keyed by relpath. `threads == 0`
/// lets the walker choose.
pub(crate) fn scan(
    wc_root: &Path,
    target: &RelPath,
    descend: HashSet<String>,
    threads: usize,
) -> Result<HashMap<String, FsEntry>, SvnError> {
    let root_path = wc_root.join(target.as_str());
    let base = wc_root.to_path_buf();
    let filter_base = base.clone();
    let descend = Arc::new(descend);
    let sink: Arc<Mutex<Collected>> = Arc::new(Mutex::new(Vec::new()));
    let errors: Arc<Mutex<Vec<ignore::Error>>> = Arc::new(Mutex::new(Vec::new()));

    let walker = WalkBuilder::new(&root_path)
        .standard_filters(false)
        .hidden(false)
        .follow_links(false)
        .threads(threads)
        .filter_entry(move |entry| {
            if entry.file_name() == ADM_DIR && entry.depth() > 0 {
                return false;
            }
            if !entry.file_type().is_some_and(|t| t.is_dir()) || entry.depth() == 0 {
                return true;
            }
            let rel = rel_string(&filter_base, entry.path());
            descend.contains(&rel)
        })
        .build_parallel();

    walker.run(|| {
        let mut batch = Batch { local: Vec::new(), sink: Arc::clone(&sink) };
        let base = base.clone();
        let errors = Arc::clone(&errors);
        Box::new(move |result| {
            match result {
                Ok(entry) => match entry.metadata() {
                    Ok(md) => batch.local.push((rel_string(&base, entry.path()), FsEntry::from_metadata(&md))),
                    // The entry vanished between readdir and lstat: treat it as absent.
                    Err(e) if e.io_error().is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) => {}
                    Err(e) => errors.lock().unwrap_or_else(PoisonError::into_inner).push(e),
                },
                Err(e) => errors.lock().unwrap_or_else(PoisonError::into_inner).push(e),
            }
            WalkState::Continue
        })
    });

    let errors = std::mem::take(&mut *errors.lock().unwrap_or_else(PoisonError::into_inner));
    if let Some(err) = errors.into_iter().find(|e| {
        !e.io_error().is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    }) {
        return Err(SvnError::new(ErrorCode::PermissionDenied, format!("cannot scan working copy: {err}")));
    }
    let entries = std::mem::take(&mut *sink.lock().unwrap_or_else(PoisonError::into_inner));
    Ok(entries.into_iter().collect())
}

/// Relpath of `path` below `base` with `/` separators (`""` for `base` itself).
fn rel_string(base: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(base).unwrap_or(path);
    let mut out = String::new();
    for comp in rel.components() {
        if let std::path::Component::Normal(c) = comp {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(&c.to_string_lossy());
        }
    }
    out
}

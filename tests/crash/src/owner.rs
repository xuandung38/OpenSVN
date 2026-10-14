//! A [`LockOwnershipStore`] in a plain file, shared by every worker process of a test run, with
//! the same liveness rules as the app's `app.sqlite` registry on Linux: an owner is alive while
//! a process with its pid exists, is not a zombie, and has the start stamp (ticks since boot,
//! `/proc/<pid>/stat` field 22) recorded with the lock. A different boot id means dead.
//!
//! The suite runs one worker per working copy at a time; the file is rewritten atomically
//! (temp file + rename) so a SIGKILL never leaves it half written.

use std::path::{Path, PathBuf};

use svn_types::{ErrorCode, LockOwnershipStore, RelPath, Result, SvnError};

/// File-backed ownership registry: one line `root \t dir \t pid \t boot \t start` per lock.
pub struct FileLockOwnership {
    path: PathBuf,
}

/// `(root, dir, pid, boot, start)`.
type Row = (String, String, u32, String, u64);

/// Boot session id of this machine (`/proc/sys/kernel/random/boot_id`).
pub fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "unknown-boot".to_owned())
}

/// `(state, start ticks)` of a live process, `None` when there is no such process.
fn proc_stat(pid: u32) -> Option<(char, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ...`: comm may contain spaces and parentheses; split after the last ')'.
    let rest = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    // Field 22 overall is index 19 after `pid (comm)`.
    let start = fields.get(19)?.parse().ok()?;
    Some((state, start))
}

fn io(e: std::io::Error) -> SvnError {
    SvnError::new(ErrorCode::Internal, format!("lock ownership file: {e}"))
}

fn root_key(wcroot: &Path) -> String {
    std::fs::canonicalize(wcroot)
        .unwrap_or_else(|_| wcroot.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

impl FileLockOwnership {
    /// Registry stored in `path` (created on first write).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn load(&self) -> Result<Vec<Row>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io(e)),
        };
        Ok(text
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                Some((
                    (*f.first()?).to_owned(),
                    (*f.get(1)?).to_owned(),
                    f.get(2)?.parse().ok()?,
                    (*f.get(3)?).to_owned(),
                    f.get(4)?.parse().ok()?,
                ))
            })
            .collect())
    }

    fn save(&self, rows: &[Row]) -> Result<()> {
        let mut text = String::new();
        for (root, dir, pid, boot, start) in rows {
            text.push_str(&format!("{root}\t{dir}\t{pid}\t{boot}\t{start}\n"));
        }
        let tmp = self
            .path
            .with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, text).map_err(io)?;
        std::fs::rename(&tmp, &self.path).map_err(io)
    }

    fn alive(pid: u32, boot: &str, start: u64) -> bool {
        if boot != boot_id() {
            return false;
        }
        matches!(proc_stat(pid), Some((state, s)) if state != 'Z' && state != 'X' && s == start)
    }
}

impl LockOwnershipStore for FileLockOwnership {
    fn record(&self, wcroot: &Path, dir: &RelPath, pid: u32, boot_id: &str) -> Result<()> {
        let root = root_key(wcroot);
        let start = proc_stat(pid).map(|(_, s)| s).unwrap_or(0);
        let mut rows = self.load()?;
        if let Some(r) = rows.iter().find(|r| r.0 == root && r.1 == dir.as_str())
            && (r.2 != pid || r.4 != start)
            && Self::alive(r.2, &r.3, r.4)
        {
            return Err(SvnError::new(
                ErrorCode::WcBusy,
                format!("'{}' is locked by live process {}", dir.as_str(), r.2),
            ));
        }
        rows.retain(|r| !(r.0 == root && r.1 == dir.as_str()));
        rows.push((
            root,
            dir.as_str().to_owned(),
            pid,
            boot_id.to_owned(),
            start,
        ));
        self.save(&rows)
    }

    fn forget(&self, wcroot: &Path, dir: &RelPath) -> Result<()> {
        let root = root_key(wcroot);
        let mut rows = self.load()?;
        let before = rows.len();
        rows.retain(|r| !(r.0 == root && r.1 == dir.as_str()));
        if rows.len() == before {
            return Ok(());
        }
        self.save(&rows)
    }

    fn owned_by_dead_process(&self, wcroot: &Path) -> Result<Vec<RelPath>> {
        let root = root_key(wcroot);
        self.load()?
            .into_iter()
            .filter(|r| r.0 == root && !Self::alive(r.2, &r.3, r.4))
            .map(|r| RelPath::new(&r.1))
            .collect()
    }
}

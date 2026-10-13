//! Bookkeeping of one update drive: configuration, per-directory and per-file state.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use svn_types::{CancelToken, Depth, PropChange, RelPath, Revnum};
use svn_wc::conflicts::{TreeAction, TreeReason};
use svn_wc::db::{Node, WcDb};
use tempfile::TempPath;

use crate::ctx::{ClientCtx, UpdateEvent};

/// Static parameters of a drive.
pub struct UpdateSetup {
    /// Working copy handle (locked by the caller for the whole drive).
    pub db: Arc<WcDb>,
    /// Working copy path of the directory the session URL points to.
    pub anchor: RelPath,
    /// Update target relative to the anchor (root: the anchor itself).
    pub target: RelPath,
    /// Repository path the target moves to (switch), else `None`.
    pub switch_relpath: Option<String>,
    /// Depth requested by the user (`Unknown`: keep what the working copy has).
    pub depth: Depth,
    /// An unversioned file that is in the way is adopted instead of becoming a tree conflict.
    pub allow_unversioned_obstructions: bool,
    /// Repository root URL as stored in wc.db.
    pub repos_root: String,
    /// Repository UUID.
    pub uuid: String,
    /// Client settings and progress sink.
    pub ctx: ClientCtx,
    /// Cancellation, polled between work items.
    pub cancel: CancelToken,
}

/// Facts the drive reports to the caller, shared with the editor running on another thread.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Notifications in order.
    pub events: Vec<UpdateEvent>,
    /// Revision announced by the server.
    pub revision: Option<Revnum>,
    /// `close_edit` ran.
    pub completed: bool,
}

/// Shared handle on the [`Outcome`].
pub type OutcomeHandle = Arc<Mutex<Outcome>>;

/// `svn:entry:*` values that feed the node row.
#[derive(Debug, Clone, Default)]
pub struct EntryProps {
    /// Last changed revision.
    pub rev: Option<u64>,
    /// Last changed date, APR microseconds.
    pub date: Option<i64>,
    /// Last author.
    pub author: Option<String>,
}

/// A text conflict-free reason to leave a node alone or record a tree conflict.
#[derive(Debug, Clone)]
pub struct PendingTreeConflict {
    /// Local state.
    pub reason: TreeReason,
    /// Incoming action.
    pub action: TreeAction,
}

/// An open directory.
pub struct DirState {
    /// Working copy path.
    pub wc: RelPath,
    /// Directory is new.
    pub added: bool,
    /// Directory lies in the part of the tree being updated (not just on the way to it).
    pub in_target: bool,
    /// Ignore every change below (conflict or obstruction).
    pub skipped: bool,
    /// Repository path after the update.
    pub new_repos: String,
    /// A switch moves this directory and everything below it.
    pub relocating: bool,
    /// BASE row at open time.
    pub base: Option<Node>,
    /// Depth recorded for the directory afterwards.
    pub depth: Depth,
    /// Incoming property changes, applied at close.
    pub prop_changes: Vec<PropChange>,
    /// Entry properties.
    pub entry: EntryProps,
    /// Tree conflict to record at close.
    pub tree_conflict: Option<PendingTreeConflict>,
}

/// Text produced by a text delta, waiting for `close_file`.
pub struct NewText {
    /// Temporary file holding the repository-normal text; removed on drop.
    pub path: TempPath,
    /// MD5 of the text.
    pub md5: [u8; 16],
    /// SHA-1 of the text.
    pub sha1: [u8; 20],
}

/// Slot a text sink fills when its delta ends.
pub type TextSlot = Arc<Mutex<Option<NewText>>>;

/// An open file.
pub struct FileState {
    /// Working copy path.
    pub wc: RelPath,
    /// File is new.
    pub added: bool,
    /// Ignore the file (conflict or obstruction already recorded).
    pub skipped: bool,
    /// Repository path after the update.
    pub new_repos: String,
    /// BASE row at open time.
    pub base: Option<Node>,
    /// An unversioned file already occupies the path and is adopted.
    pub adopt_existing: bool,
    /// Incoming property changes.
    pub prop_changes: Vec<PropChange>,
    /// Entry properties.
    pub entry: EntryProps,
    /// Text delta result.
    pub text: TextSlot,
    /// Tree conflict to record when the file closes.
    pub tree_conflict: Option<PendingTreeConflict>,
}

/// Path of the temporary directory of the administrative area.
pub fn tmp_dir(db: &WcDb) -> PathBuf {
    db.wcroot().join(svn_wc::db::ADM_DIR).join("tmp")
}

/// Parses an svn date (`2026-10-13T07:12:26.201123Z`) into APR microseconds.
pub fn parse_svn_date(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hms.split(':');
    let (hh, mm, ss): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    let mut micros: i64 = 0;
    let mut digits = 0;
    for c in frac.chars().take(6) {
        micros = micros * 10 + i64::from(c.to_digit(10)?);
        digits += 1;
    }
    while digits < 6 {
        micros *= 10;
        digits += 1;
    }
    // Days from civil (Howard Hinnant).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + hh * 3600 + mm * 60 + ss) * 1_000_000) + micros)
}

#[cfg(test)]
mod tests {
    use super::parse_svn_date;

    #[test]
    fn parses_svn_timestamps() {
        assert_eq!(parse_svn_date("1970-01-01T00:00:00.000000Z"), Some(0));
        assert_eq!(
            parse_svn_date("2026-10-13T07:12:26.201123Z"),
            Some(1_791_875_546_201_123)
        );
        assert_eq!(parse_svn_date("garbage"), None);
    }
}

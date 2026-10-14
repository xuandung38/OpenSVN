//! Bookkeeping of one update drive: configuration, per-directory and per-file state.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use svn_types::{CancelToken, Depth, PropChange, RelPath, Revnum};
use svn_wc::conflicts::{TreeAction, TreeReason};
use svn_wc::db::{NewExternal, Node, WcDb};
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
    /// Depth requested by the user (`Unknown`: the depth the working copy has). Like
    /// `svn update --depth`, it limits the drive and is never recorded: the working copy keeps
    /// its depths (checkout records the depth up front).
    pub depth: Depth,
    /// `svn update --set-depth`: the requested depth becomes the recorded depth of the target
    /// (and of every directory below it when it is `Infinity`), and the drive may bring in what
    /// the recorded depths left out.
    pub sticky_depth: bool,
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
    /// The target is the file external described here: its BASE row is marked as a file
    /// external and the `EXTERNALS` row is written in the same transaction as the file.
    pub file_external: Option<NewExternal>,
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
    /// How far below this directory the drive applies (`Empty`: nothing, `Files`: files only,
    /// `Immediates`: children, whose own children are left alone). The requested depth of the
    /// update narrowed by the depth the working copy already has; never recorded.
    pub limit: Depth,
    /// Children dropped by `limit` that the server adds become `not-present` rows: the limit
    /// comes from a requested depth narrower than the recorded one (svn issue #3569).
    pub not_present_on_drop: bool,
    /// The directory is shadowed (it lies under an unversioned obstruction, a local delete or
    /// replace, or it is an incoming replacement of a locally changed node): BASE is written,
    /// the disk is left alone and every node gets a `base-deleted` row at this op depth. 0: no
    /// working layer to extend (an incoming replacement of a node deleted locally), BASE only.
    pub shadow: Option<u32>,
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
    /// The file is shadowed: BASE only, with a `base-deleted` row at this op depth unless it
    /// is 0 (see [`DirState::shadow`]).
    pub shadow: Option<u32>,
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

/// Formats APR microseconds as an svn date (`2026-10-14T02:22:00.000000Z`), the inverse of
/// [`parse_svn_date`].
pub fn format_svn_date(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{frac:06}Z",
        tod / 3600,
        tod % 3600 / 60,
        tod % 60
    )
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{format_svn_date, parse_svn_date};

    #[test]
    fn formats_what_it_parses() {
        for s in [
            "1970-01-01T00:00:00.000000Z",
            "2000-02-29T23:59:59.999999Z",
            "2026-10-14T02:22:07.123456Z",
            "1969-12-31T23:59:59.500000Z",
        ] {
            let t = parse_svn_date(s).expect("parse");
            assert_eq!(format_svn_date(t), s);
        }
    }

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

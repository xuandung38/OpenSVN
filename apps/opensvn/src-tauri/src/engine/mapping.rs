//! Conversions from engine types to the IPC model, shared by the fake and the real engine.

use opensvn_ipc::time_fmt::timestamp_to_iso;
use opensvn_ipc::{ChangeAction, ChangedPath, CopyFrom, EntryKind, LogEntry, PathKind, Rev};
use svn_types::{NodeKind, Revnum};

/// IPC revision from an engine revision.
pub(crate) fn rev(r: Revnum) -> Rev {
    Rev(r.0)
}

/// Engine revision from an IPC revision.
pub(crate) fn revnum(r: Option<Rev>) -> Option<Revnum> {
    r.map(|r| Revnum(r.0))
}

/// Entry kind of a listed node (anything that is not a directory is shown as a file).
pub(crate) fn entry_kind(kind: NodeKind) -> EntryKind {
    if kind == NodeKind::Dir {
        EntryKind::Dir
    } else {
        EntryKind::File
    }
}

/// IPC lock description of a repository lock seen in a listing: whether it is this working
/// copy's own lock is only known from the working copy, so the token is not exposed here.
pub(crate) fn lock(l: svn_types::LockInfo) -> opensvn_ipc::LockInfo {
    opensvn_ipc::LockInfo {
        owner: l.owner,
        comment: l.comment,
        created_at: timestamp_to_iso(l.created),
        token: None,
        mine: false,
    }
}

/// IPC log entry.
pub(crate) fn log_entry(e: svn_types::LogEntry) -> LogEntry {
    let paths: Vec<ChangedPath> = e
        .changed_paths
        .into_iter()
        .map(|c| ChangedPath {
            path: c.path.to_string(),
            action: match c.action {
                svn_types::ChangeAction::Added => ChangeAction::A,
                svn_types::ChangeAction::Modified => ChangeAction::M,
                svn_types::ChangeAction::Deleted => ChangeAction::D,
                svn_types::ChangeAction::Replaced => ChangeAction::R,
            },
            kind: match c.kind {
                NodeKind::File => PathKind::File,
                NodeKind::Dir => PathKind::Dir,
                _ => PathKind::Unknown,
            },
            copy_from: c.copy_from.map(|(path, r)| CopyFrom {
                path: path.to_string(),
                rev: rev(r),
            }),
            text_mods: Some(c.text_mods),
            prop_mods: Some(c.prop_mods),
        })
        .collect();
    LogEntry {
        rev: rev(e.rev),
        author: e.author,
        date: e.date.map_or_else(String::new, timestamp_to_iso),
        message: e.message.unwrap_or_default(),
        changed_paths: (!paths.is_empty()).then_some(paths),
        has_children: Some(e.has_children),
        merged: None,
        subtractive_merge: e.subtractive_merge.then_some(true),
        revprops: (!e.revprops.is_empty()).then(|| {
            e.revprops
                .into_iter()
                .map(|(k, v)| (k, String::from_utf8_lossy(&v).into_owned()))
                .collect()
        }),
    }
}

/// One page of a log as the engines read it: the text filter (message and author, case
/// insensitive) and the limit apply to the node's own entries; the revisions a matching entry
/// merged in come along with it.
pub(crate) struct LogPager {
    filter: Option<String>,
    limit: usize,
    /// The current own entry matched, so its merged revisions are kept.
    keep: bool,
    tops: usize,
    entries: Vec<svn_types::LogEntry>,
    next: Option<svn_types::Revnum>,
}

impl LogPager {
    pub(crate) fn new(req: &opensvn_ipc::LogGetRequest) -> Self {
        Self {
            filter: req
                .filter
                .as_ref()
                .map(|f| f.to_lowercase())
                .filter(|f| !f.is_empty()),
            limit: req.limit.clamp(1, 1000) as usize,
            keep: false,
            tops: 0,
            entries: Vec::new(),
            next: None,
        }
    }

    /// True when a text filter decides which entries match (the server cannot stop early).
    pub(crate) fn filtered(&self) -> bool {
        self.filter.is_some()
    }

    /// Takes the next entry of the report; false once the page is complete (one own entry
    /// beyond it proves a next page, and reading can stop).
    pub(crate) fn accept(&mut self, e: svn_types::LogEntry) -> bool {
        if e.merge_depth > 0 {
            if self.keep {
                self.entries.push(e);
            }
            return true;
        }
        self.keep = self.filter.as_ref().is_none_or(|f| {
            e.message
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains(f)
                || e.author.as_deref().unwrap_or("").to_lowercase().contains(f)
        });
        if !self.keep {
            return true;
        }
        if self.tops == self.limit {
            self.keep = false;
            self.next = Some(e.rev);
            return false;
        }
        self.tops += 1;
        self.entries.push(e);
        true
    }

    /// Own entries taken so far.
    pub(crate) fn taken(&self) -> usize {
        self.tops
    }

    /// Ends the page early, before the report is complete (a newest-first report the server is
    /// slow to finish): the next page starts below the last own entry taken. An entry whose
    /// merged revisions may still have been arriving is left for the next page unless it is the
    /// only one. `floor` is the oldest revision of the range.
    pub(crate) fn cut(&mut self, floor: u64) {
        let Some(last) = self.entries.iter().rposition(|e| e.merge_depth == 0) else {
            return;
        };
        let top = &self.entries[last];
        if top.has_children && self.tops > 1 {
            self.next = Some(top.rev);
            self.entries.truncate(last);
            self.tops -= 1;
        } else {
            let below = top.rev.0.saturating_sub(1);
            self.next = (below >= floor.max(1)).then_some(svn_types::Revnum(below));
        }
    }

    pub(crate) fn page(self) -> opensvn_ipc::LogPage {
        opensvn_ipc::LogPage {
            entries: log_entries(self.entries),
            next_rev: self.next.map(rev),
        }
    }
}

/// Revision properties a log request reads: the usual three, plus those asked for.
pub(crate) fn log_revprops(req: &opensvn_ipc::LogGetRequest) -> svn_ra_api::LogRevprops {
    match &req.revprops {
        None => svn_ra_api::LogRevprops::Default,
        Some(extra) => {
            let mut names: Vec<String> = ["svn:author", "svn:date", "svn:log"]
                .into_iter()
                .map(str::to_owned)
                .collect();
            for n in extra {
                if !names.contains(n) {
                    names.push(n.clone());
                }
            }
            svn_ra_api::LogRevprops::Only(names)
        }
    }
}

/// [`log_entry`] of entries in report order, with the revisions a merge brought in (`log -g`,
/// `merge_depth > 0`, right after their merging entry) nested under it in `merged`.
pub(crate) fn log_entries(entries: Vec<svn_types::LogEntry>) -> Vec<LogEntry> {
    /// Entries still open for children, deepest last, with their depth.
    fn close(stack: &mut Vec<(u32, LogEntry)>, out: &mut Vec<LogEntry>, depth: u32) {
        while stack.last().is_some_and(|(d, _)| *d >= depth) {
            let Some((_, done)) = stack.pop() else { break };
            match stack.last_mut() {
                Some((_, parent)) => parent.merged.get_or_insert_with(Vec::new).push(done),
                None => out.push(done),
            }
        }
    }
    let mut out = Vec::new();
    let mut stack: Vec<(u32, LogEntry)> = Vec::new();
    for e in entries {
        // A child deeper than its parent's level + 1 (a malformed report) hangs under the
        // deepest open entry.
        let depth = e.merge_depth.min(stack.last().map_or(0, |(d, _)| d + 1));
        close(&mut stack, &mut out, depth);
        stack.push((depth, log_entry(e)));
    }
    close(&mut stack, &mut out, 0);
    out
}

/// The folder depths of a sparse checkout or "Choose folders", parents first; a folder given
/// twice is refused. The root (`""`) may only be listed by "Choose folders" (`with_root`; a
/// checkout's root takes the checkout depth) and never be excluded.
pub(crate) fn sparse_steps(
    entries: &[opensvn_ipc::SparseEntry],
    with_root: bool,
) -> Result<Vec<(svn_types::RelPath, opensvn_ipc::SparseDepth)>, svn_types::SvnError> {
    let mut steps = entries
        .iter()
        .map(|e| {
            let p = svn_types::RelPath::new(&e.path)?;
            if p.is_root() && !with_root {
                return Err(svn_types::SvnError::new(
                    svn_types::ErrorCode::WcInvalidOperation,
                    "the depth of the checkout root is the checkout depth, not a folder depth",
                ));
            }
            if p.is_root() && e.depth == opensvn_ipc::SparseDepth::Exclude {
                return Err(svn_types::SvnError::new(
                    svn_types::ErrorCode::WcInvalidOperation,
                    "the working copy root cannot be excluded",
                ));
            }
            Ok((p, e.depth))
        })
        .collect::<Result<Vec<_>, svn_types::SvnError>>()?;
    steps.sort_by_cached_key(|(p, _)| p.components().map(str::to_owned).collect::<Vec<_>>());
    if let Some(w) = steps.windows(2).find(|w| w[0].0 == w[1].0) {
        return Err(svn_types::SvnError::new(
            svn_types::ErrorCode::WcInvalidOperation,
            format!("the folder '{}' is listed twice", w[0].0),
        ));
    }
    Ok(steps)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use svn_types::{RepoDisplayPath, Revnum};

    use super::*;

    fn raw(rev: u64, depth: u32, has_children: bool) -> svn_types::LogEntry {
        svn_types::LogEntry {
            rev: Revnum(rev),
            author: None,
            date: None,
            message: Some(format!("r{rev}")),
            changed_paths: Vec::new(),
            has_children,
            merge_depth: depth,
            subtractive_merge: false,
            revprops: Vec::new(),
        }
    }

    fn request(limit: u32) -> opensvn_ipc::LogGetRequest {
        opensvn_ipc::LogGetRequest {
            target: opensvn_ipc::LogTarget::Url { url: "u".into() },
            from_rev: None,
            to_rev: None,
            limit,
            stop_on_copy: None,
            with_paths: false,
            filter: None,
            include_merged: Some(true),
            revprops: None,
        }
    }

    #[test]
    fn merged_revisions_nest_under_the_merging_entry_and_do_not_count_against_the_limit() {
        let mut p = LogPager::new(&request(2));
        for e in [
            raw(9, 0, true),
            raw(7, 1, true),
            raw(5, 2, false),
            raw(6, 1, false),
            raw(8, 0, false),
        ] {
            assert!(p.accept(e));
        }
        assert!(!p.accept(raw(4, 0, false)));
        let page = p.page();
        assert_eq!(page.next_rev, Some(Rev(4)));
        assert_eq!(page.entries.len(), 2);
        let merged = page.entries[0].merged.as_ref().unwrap();
        assert_eq!(merged.iter().map(|e| e.rev.0).collect::<Vec<_>>(), [7, 6]);
        assert_eq!(merged[0].merged.as_ref().unwrap()[0].rev, Rev(5));
        assert!(page.entries[1].merged.is_none());
    }

    #[test]
    fn a_page_cut_short_continues_below_the_last_entry_and_never_splits_a_merge() {
        let mut p = LogPager::new(&request(100));
        for e in [raw(9, 0, false), raw(8, 0, false)] {
            p.accept(e);
        }
        p.cut(1);
        assert_eq!(p.page().next_rev, Some(Rev(7)));

        // The last entry's merged revisions may be incomplete: it moves to the next page.
        let mut p = LogPager::new(&request(100));
        for e in [raw(9, 0, false), raw(8, 0, true), raw(3, 1, false)] {
            p.accept(e);
        }
        p.cut(1);
        let page = p.page();
        assert_eq!(page.next_rev, Some(Rev(8)));
        assert_eq!(page.entries.len(), 1);

        // Nothing older than the range's floor: the page is complete.
        let mut p = LogPager::new(&request(100));
        p.accept(raw(5, 0, false));
        p.cut(5);
        assert_eq!(p.page().next_rev, None);
    }

    #[test]
    fn log_entries_carry_display_paths_as_text() {
        let entry = svn_types::LogEntry {
            rev: Revnum(7),
            author: Some("alice".into()),
            date: None,
            message: None,
            changed_paths: vec![svn_types::ChangedPath {
                path: RepoDisplayPath::parse("/branches/a b/x.txt").unwrap(),
                action: svn_types::ChangeAction::Added,
                kind: NodeKind::File,
                copy_from: Some((RepoDisplayPath::parse("/trunk/x.txt").unwrap(), Revnum(5))),
                text_mods: true,
                prop_mods: false,
            }],
            has_children: false,
            merge_depth: 0,
            subtractive_merge: false,
            revprops: Vec::new(),
        };
        let mapped = log_entry(entry);
        assert_eq!(mapped.rev, Rev(7));
        assert_eq!(mapped.message, "");
        assert_eq!(mapped.date, "");
        let paths = mapped.changed_paths.unwrap();
        assert_eq!(paths[0].path, "/branches/a b/x.txt");
        assert_eq!(paths[0].action, ChangeAction::A);
        assert_eq!(paths[0].kind, PathKind::File);
        let from = paths[0].copy_from.as_ref().unwrap();
        assert_eq!((from.path.as_str(), from.rev), ("/trunk/x.txt", Rev(5)));
    }
}

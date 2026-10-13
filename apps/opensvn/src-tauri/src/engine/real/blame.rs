//! Blame from `file_revs`: every revision of a file arrives as a text delta against the previous
//! one; the engine rebuilds each text and attributes lines to the revision that introduced them.
//!
//! Memory and time are bounded: every rebuilt text is capped at [`BLAME_SIZE_LIMIT`] bytes and
//! each line comparison runs `svn-diff` under [`BLAME_MAX_COST`]; going over either limit fails
//! with "file is too large to annotate" instead of exhausting memory.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use opensvn_ipc::{BlameLine, Rev};
use svn_delta::WindowApplier;
use svn_diff::{DiffError, DiffOptions, DisplayOptions};
use svn_ra_api::{FileRev, FileRevHandler};
use svn_types::{CancelToken, ErrorCode, Result, SvnError, TxDeltaWindow, WindowSink};

use super::RealEngine;
use super::remote::{CAT_LIMIT, internal};
use super::text::SharedBuf;
use crate::engine::context::{OpContext, cancelled};
use crate::engine::facade::EngineResult;
use crate::engine::mapping::{self, revnum};
use crate::engine::text::is_binary;

/// Largest text of any revision blame rebuilds (same cap as showing a file).
pub(super) const BLAME_SIZE_LIMIT: usize = CAT_LIMIT;
/// Work budget of one revision-to-revision line comparison (`svn-diff` LCS steps).
pub(super) const BLAME_MAX_COST: u64 = svn_diff::DISPLAY_MAX_COST;

fn too_large() -> SvnError {
    internal("file is too large to annotate")
}

/// A revision whose delta is being received.
struct Pending {
    /// Attribution every line this revision introduces gets.
    attribution: Attribution,
    out: SharedBuf,
    /// Set once the closing `window(None)` arrived, i.e. the delta is complete.
    complete: Arc<AtomicBool>,
}

/// Window sink that remembers whether the delta was closed.
struct Tracked<S> {
    inner: S,
    complete: Arc<AtomicBool>,
}

impl<S: WindowSink> WindowSink for Tracked<S> {
    fn window(&mut self, w: Option<&TxDeltaWindow>) -> Result<()> {
        self.inner.window(w)?;
        if w.is_none() {
            self.complete.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// Attribution of a line: the revision that introduced it. Author and date are shared by every
/// line of that revision (and authors across revisions), so a line costs a few pointers instead
/// of two heap strings.
#[derive(Debug, Clone)]
struct Attribution {
    rev: Rev,
    author: Option<Arc<str>>,
    date: Arc<str>,
}

/// Line without its terminator (`\n`, `\r\n` or `\r`, the same terminators `svn-diff` splits on).
fn strip_eol(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Collects `file_revs` results into per-line attribution.
struct BlameCollector {
    previous: Arc<[u8]>,
    lines: Vec<Attribution>,
    /// Interned author names, one allocation per distinct author.
    authors: HashMap<String, Arc<str>>,
    pending: Option<Pending>,
    size_limit: usize,
    max_cost: u64,
    cancel: CancelToken,
}

impl BlameCollector {
    fn new(cancel: CancelToken) -> Self {
        Self::with_limits(cancel, BLAME_SIZE_LIMIT, BLAME_MAX_COST)
    }

    fn with_limits(cancel: CancelToken, size_limit: usize, max_cost: u64) -> Self {
        Self {
            previous: Arc::from(Vec::new()),
            lines: Vec::new(),
            authors: HashMap::new(),
            pending: None,
            size_limit,
            max_cost,
            cancel,
        }
    }

    fn intern_author(&mut self, name: String) -> Arc<str> {
        if let Some(shared) = self.authors.get(&name) {
            return shared.clone();
        }
        let shared: Arc<str> = Arc::from(name.as_str());
        self.authors.insert(name, shared.clone());
        shared
    }

    /// Whether the revision being received went over the size limit.
    fn exceeded(&self) -> bool {
        self.pending.as_ref().is_some_and(|p| p.out.exceeded())
    }

    /// Completes the revision whose delta was just received.
    fn settle(&mut self) -> Result<()> {
        let Some(p) = self.pending.take() else {
            return Ok(());
        };
        if p.out.exceeded() {
            return Err(too_large());
        }
        // A revision that changed only properties may get a sink that never sees a window, not
        // even the closing one (see `FileRevHandler`): the text and its attribution stay.
        if !p.complete.load(Ordering::SeqCst) {
            return Ok(());
        }
        let data = p.out.take();
        if is_binary(&data) {
            return Err(internal("cannot annotate a binary file"));
        }
        let opts = DisplayOptions {
            diff: DiffOptions {
                max_cost: Some(self.max_cost),
                ..DiffOptions::default()
            },
            context: 0,
        };
        let hunks =
            svn_diff::diff(&self.previous, &data, &opts, &self.cancel).map_err(|e| match e {
                DiffError::Cancelled => cancelled(),
                DiffError::TooComplex => too_large(),
            })?;
        let new_count = svn_diff::split_lines(&data).len();
        let fresh = || p.attribution.clone();
        let carried = std::mem::take(&mut self.lines);
        let mut lines = Vec::with_capacity(new_count);
        let (mut old, mut new) = (0usize, 0usize);
        for h in &hunks {
            // Unchanged lines before the hunk keep their attribution.
            while new < h.new_start {
                lines.push(carried.get(old).cloned().unwrap_or_else(fresh));
                old += 1;
                new += 1;
            }
            for _ in 0..h.new_len {
                lines.push(fresh());
            }
            old = h.old_start + h.old_len;
            new = h.new_start + h.new_len;
        }
        while new < new_count {
            lines.push(carried.get(old).cloned().unwrap_or_else(fresh));
            old += 1;
            new += 1;
        }
        self.lines = lines;
        self.previous = Arc::from(data);
        Ok(())
    }

    /// Final annotation, first line first.
    fn finish(mut self) -> Result<Vec<BlameLine>> {
        self.settle()?;
        let texts = svn_diff::split_lines(&self.previous);
        Ok(self
            .lines
            .into_iter()
            .zip(texts)
            .map(|(a, text)| BlameLine {
                rev: a.rev,
                author: a.author.as_deref().map(str::to_owned),
                date: a.date.to_string(),
                text: String::from_utf8_lossy(strip_eol(text)).into_owned(),
            })
            .collect())
    }
}

fn rev_prop(rev: &FileRev, name: &str) -> Option<String> {
    rev.rev_props
        .iter()
        .find(|p| p.name == name)
        .and_then(|p| p.value.as_ref())
        .map(|v| String::from_utf8_lossy(v).into_owned())
}

impl FileRevHandler for BlameCollector {
    fn revision(&mut self, rev: FileRev) -> Result<Option<Box<dyn WindowSink>>> {
        self.settle()?;
        // Revisions that only arrived through a merge do not change who wrote a line here.
        if rev.merged {
            return Ok(None);
        }
        let out = SharedBuf::with_limit(self.size_limit);
        let complete = Arc::new(AtomicBool::new(false));
        let applier = WindowApplier::new(Cursor::new(self.previous.clone()), out.clone());
        let author = rev_prop(&rev, "svn:author").map(|name| self.intern_author(name));
        self.pending = Some(Pending {
            attribution: Attribution {
                rev: mapping::rev(rev.rev),
                author,
                date: Arc::from(rev_prop(&rev, "svn:date").unwrap_or_default()),
            },
            out,
            complete: complete.clone(),
        });
        Ok(Some(Box::new(Tracked {
            inner: applier,
            complete,
        })))
    }
}

impl RealEngine {
    pub(super) async fn do_blame(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<Vec<BlameLine>> {
        let (session, path) = self.locate(url, ctx).await?;
        let head = match revnum(rev) {
            Some(r) => r,
            None => session.youngest_rev(&ctx.cancel).await?,
        };
        let mut collector = BlameCollector::new(ctx.cancel.clone());
        let received = session
            .file_revs(&path, None, Some(head), &mut collector, &ctx.cancel)
            .await;
        if collector.exceeded() {
            return Err(too_large());
        }
        received?;
        let lines = collector.finish()?;
        if lines.is_empty() {
            // A file with no content still exists; a missing one is an error of its own.
            session
                .check_path(&path, Some(head), &ctx.cancel)
                .await
                .and_then(|kind| {
                    if kind == svn_types::NodeKind::File {
                        Ok(())
                    } else {
                        Err(SvnError::new(
                            ErrorCode::PathNotFound,
                            format!("{url} is not a file at r{}", head.0),
                        ))
                    }
                })?;
        }
        Ok(lines)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use bytes::Bytes;
    use svn_types::{PropChange, RepoDisplayPath, Revnum};

    use super::*;

    fn file_rev(n: u64, author: &str) -> FileRev {
        FileRev {
            path: RepoDisplayPath::parse("/trunk/a.txt").unwrap(),
            rev: Revnum(n),
            rev_props: vec![PropChange {
                name: "svn:author".into(),
                value: Some(Bytes::from(author.to_owned())),
            }],
            merged: false,
        }
    }

    /// Sends `text` as a full-text delta for revision `n`.
    fn send(c: &mut BlameCollector, n: u64, author: &str, text: &str) -> Result<()> {
        let mut sink = c.revision(file_rev(n, author))?.expect("sink");
        // Several windows, as a server sends a large text.
        for chunk in text.as_bytes().chunks(16 * 1024) {
            sink.window(Some(&TxDeltaWindow::full_text(Bytes::copy_from_slice(
                chunk,
            ))))?;
        }
        sink.window(None)
    }

    fn revs(lines: &[BlameLine]) -> Vec<(u64, &str)> {
        lines.iter().map(|l| (l.rev.0, l.text.as_str())).collect()
    }

    #[test]
    fn lines_keep_the_revision_that_introduced_them() {
        let mut c = BlameCollector::new(CancelToken::new());
        send(&mut c, 1, "alice", "a\nb\nc\n").unwrap();
        send(&mut c, 2, "bob", "a\nx\nc\nd\n").unwrap();
        send(&mut c, 3, "carol", "new\na\nx\nc\r\nd").unwrap();
        let lines = c.finish().unwrap();
        assert_eq!(
            revs(&lines),
            vec![(3, "new"), (1, "a"), (2, "x"), (3, "c"), (3, "d")]
        );
        assert_eq!(lines[1].author.as_deref(), Some("alice"));
    }

    #[test]
    fn lines_of_one_revision_share_author_and_date() {
        let mut c = BlameCollector::new(CancelToken::new());
        let text: String = (0..1_000).map(|i| format!("line {i}\n")).collect();
        send(&mut c, 1, "alice", &text).unwrap();
        send(&mut c, 2, "bob", &format!("{text}tail\n")).unwrap();
        send(&mut c, 3, "alice", &format!("head\n{text}tail\n")).unwrap();
        c.settle().unwrap();
        let lines = &c.lines;
        assert_eq!(lines.len(), 1_002);
        let first = &lines[1];
        assert_eq!(first.rev, Rev(1));
        for line in &lines[2..1_001] {
            assert!(Arc::ptr_eq(
                line.author.as_ref().unwrap(),
                first.author.as_ref().unwrap()
            ));
            assert!(Arc::ptr_eq(&line.date, &first.date));
        }
        // One allocation per author across revisions too.
        let (head, tail) = (&lines[0], &lines[1_001]);
        assert_eq!((head.rev, tail.rev), (Rev(3), Rev(2)));
        assert!(Arc::ptr_eq(
            head.author.as_ref().unwrap(),
            first.author.as_ref().unwrap()
        ));
        assert_eq!(tail.author.as_deref(), Some("bob"));
        assert_eq!(c.authors.len(), 2);
    }

    #[test]
    fn a_revision_without_any_window_keeps_text_and_attribution() {
        let mut c = BlameCollector::new(CancelToken::new());
        send(&mut c, 1, "alice", "one\ntwo\n").unwrap();
        // Property-only change: the sink is handed out but never receives a window.
        let sink = c.revision(file_rev(2, "bob")).unwrap().expect("sink");
        drop(sink);
        send(&mut c, 3, "carol", "one\ntwo\nthree\n").unwrap();
        let lines = c.finish().unwrap();
        assert_eq!(revs(&lines), vec![(1, "one"), (1, "two"), (3, "three")]);
    }

    #[test]
    fn a_trailing_revision_without_any_window_keeps_the_text() {
        let mut c = BlameCollector::new(CancelToken::new());
        send(&mut c, 1, "alice", "one\n").unwrap();
        drop(c.revision(file_rev(2, "bob")).unwrap());
        assert_eq!(revs(&c.finish().unwrap()), vec![(1, "one")]);
    }

    #[test]
    fn an_explicitly_emptied_file_has_no_lines() {
        let mut c = BlameCollector::new(CancelToken::new());
        send(&mut c, 1, "alice", "one\n").unwrap();
        send(&mut c, 2, "bob", "").unwrap();
        assert!(c.finish().unwrap().is_empty());
    }

    #[test]
    fn a_revision_over_the_size_limit_is_refused() {
        let mut c = BlameCollector::with_limits(CancelToken::new(), 8, BLAME_MAX_COST);
        send(&mut c, 1, "alice", "short\n").unwrap();
        assert!(send(&mut c, 2, "bob", "far too long for the cap\n").is_err());
        assert!(c.exceeded());
        let err = c.finish().unwrap_err();
        assert!(err.message.contains("too large to annotate"), "{err}");
    }

    #[test]
    fn a_comparison_over_the_cost_budget_is_refused() {
        let mut c = BlameCollector::with_limits(CancelToken::new(), BLAME_SIZE_LIMIT, 10);
        let old: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let new: String = (0..200).rev().map(|i| format!("line {i}\n")).collect();
        send(&mut c, 1, "alice", &old).unwrap();
        send(&mut c, 2, "bob", &new).unwrap();
        let err = c.finish().unwrap_err();
        assert!(err.message.contains("too large to annotate"), "{err}");
    }

    #[test]
    fn a_large_ordinary_history_stays_within_the_budget() {
        let mut c = BlameCollector::new(CancelToken::new());
        let base: Vec<String> = (0..20_000).map(|i| format!("line {i}\n")).collect();
        send(&mut c, 1, "alice", &base.concat()).unwrap();
        let mut edited = base.clone();
        edited[10_000] = "changed\n".into();
        send(&mut c, 2, "bob", &edited.concat()).unwrap();
        let lines = c.finish().unwrap();
        assert_eq!(lines.len(), 20_000);
        assert_eq!(lines[10_000].rev, Rev(2));
        assert_eq!(lines[9_999].rev, Rev(1));
        assert_eq!(lines[10_001].rev, Rev(1));
    }
}

//! Blame from `file_revs`: every revision of a file arrives as a text delta against the previous
//! one; the engine rebuilds each text and attributes lines to the revision that introduced them.

use std::io::Cursor;
use std::sync::Arc;

use opensvn_ipc::{BlameLine, Rev};
use svn_delta::WindowApplier;
use svn_ra_api::{FileRev, FileRevHandler};
use svn_types::{ErrorCode, Result, SvnError, WindowSink};

use super::RealEngine;
use super::mapping::{self, revnum};
use super::remote::internal;
use super::text::{SharedBuf, is_binary, match_lines};
use crate::engine::context::OpContext;
use crate::engine::facade::EngineResult;

/// A revision whose delta is being received.
struct Pending {
    rev: Rev,
    author: Option<String>,
    date: String,
    out: SharedBuf,
}

/// Attribution of each line of the current text.
type Attribution = (Rev, Option<String>, String);

/// Collects `file_revs` results into per-line attribution.
#[derive(Default)]
struct BlameCollector {
    previous: Arc<[u8]>,
    previous_lines: Vec<String>,
    lines: Vec<Attribution>,
    pending: Option<Pending>,
}

impl BlameCollector {
    /// Completes the revision whose delta was just received.
    fn settle(&mut self) -> Result<()> {
        let Some(p) = self.pending.take() else {
            return Ok(());
        };
        let data = p.out.take();
        if is_binary(&data) {
            return Err(internal("cannot annotate a binary file"));
        }
        let text = String::from_utf8_lossy(&data).into_owned();
        let current: Vec<String> = text.lines().map(str::to_owned).collect();
        let old: Vec<&str> = self.previous_lines.iter().map(String::as_str).collect();
        let new: Vec<&str> = current.iter().map(String::as_str).collect();
        let matches = match_lines(&old, &new);
        let carried = std::mem::take(&mut self.lines);
        self.lines = matches
            .into_iter()
            .map(|m| match m.and_then(|i| carried.get(i)) {
                Some(kept) => kept.clone(),
                None => (p.rev, p.author.clone(), p.date.clone()),
            })
            .collect();
        self.previous = Arc::from(data);
        self.previous_lines = current;
        Ok(())
    }

    /// Final annotation, oldest text first line first.
    fn finish(mut self) -> Result<Vec<BlameLine>> {
        self.settle()?;
        Ok(self
            .lines
            .into_iter()
            .zip(self.previous_lines)
            .map(|((rev, author, date), text)| BlameLine {
                rev,
                author,
                date,
                text,
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
        let out = SharedBuf::default();
        let applier = WindowApplier::new(Cursor::new(self.previous.clone()), out.clone());
        self.pending = Some(Pending {
            rev: mapping::rev(rev.rev),
            author: rev_prop(&rev, "svn:author"),
            date: rev_prop(&rev, "svn:date").unwrap_or_default(),
            out,
        });
        Ok(Some(Box::new(applier)))
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
        let mut collector = BlameCollector::default();
        session
            .file_revs(&path, None, Some(head), &mut collector, &ctx.cancel)
            .await?;
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

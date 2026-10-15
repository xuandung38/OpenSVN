//! Remote operation: file revisions (the input of blame). One `file-revs-report` REPORT.
//!
//! For each revision that changed the file's text the handler is asked for a sink; the
//! revision's svndiff (a delta against the previous revision in the report, the first one
//! against the empty file) is decoded window by window into it. Revisions that only changed
//! properties carry no delta and are not reported, like the in-memory fake session.

use svn_delta::SvndiffDecoder;
use svn_protocol::file_revs::{FileRevEvent, FileRevsParser, file_revs_request};
use svn_ra_api::{FileRev, FileRevHandler};
use svn_types::{
    CancelToken, ErrorCode, PropChange, RelPath, Result, Revnum, SvnError, WindowSink,
};

use super::read::{corrupt, peg, stream_items};
use super::resume::{ReadRetry, cannot_resume};
use super::update::RaWire;

/// Where the report currently is.
struct Drive<'h> {
    handler: &'h mut dyn FileRevHandler,
    /// Revision announced but not yet handed to the handler.
    pending: Option<FileRev>,
    /// Sink receiving the current delta, if the handler wanted it.
    sink: Option<(SvndiffDecoder, Box<dyn WindowSink>)>,
    in_delta: bool,
    /// Revisions handed to the handler, over every attempt.
    delivered: usize,
    /// Revisions of this attempt still to skip: an earlier attempt handed them over.
    skip: usize,
}

impl Drive<'_> {
    fn handle(&mut self, ev: FileRevEvent) -> Result<()> {
        match ev {
            FileRevEvent::Start { path, rev } => {
                self.pending = Some(FileRev {
                    path,
                    rev,
                    rev_props: Vec::new(),
                    merged: false,
                });
            }
            FileRevEvent::RevProp { name, value } => {
                if let Some(r) = self.pending.as_mut() {
                    r.rev_props.push(PropChange {
                        name,
                        value: Some(value),
                    });
                }
            }
            FileRevEvent::MergedRevision => {
                if let Some(r) = self.pending.as_mut() {
                    r.merged = true;
                }
            }
            FileRevEvent::SetProp { .. } | FileRevEvent::RemoveProp { .. } => {}
            FileRevEvent::TxDeltaStart => {
                let rev = self
                    .pending
                    .take()
                    .ok_or_else(|| corrupt("file-revs delta outside a revision"))?;
                self.in_delta = true;
                if self.skip > 0 {
                    // The handler has it (and the next delta builds on its own copy).
                    self.skip -= 1;
                    return Ok(());
                }
                self.delivered += 1;
                self.sink = self
                    .handler
                    .revision(rev)?
                    .map(|s| (SvndiffDecoder::new(), s));
            }
            FileRevEvent::TxDeltaChunk(bytes) => {
                if !self.in_delta {
                    return Err(corrupt("file-revs delta data outside a delta"));
                }
                if let Some((decoder, sink)) = self.sink.as_mut() {
                    decoder.feed_sink(&bytes, sink.as_mut())?;
                }
            }
            FileRevEvent::TxDeltaEnd => {
                self.in_delta = false;
                if let Some((mut decoder, mut sink)) = self.sink.take() {
                    decoder.finish_sink(sink.as_mut())?;
                }
            }
            FileRevEvent::End => {
                if self.in_delta {
                    return Err(corrupt("file-revs revision ended inside its delta"));
                }
                // A property-only revision: nothing to hand over.
                self.pending = None;
            }
        }
        Ok(())
    }
}

/// Reports every text revision of `path` in `start..=end` (`start` defaults to 0, `end` to
/// HEAD). A descending range needs the server's `reverse-file-revs` capability.
pub(crate) async fn file_revs(
    wire: &RaWire,
    path: &RelPath,
    start: Option<Revnum>,
    end: Option<Revnum>,
    handler: &mut dyn FileRevHandler,
    cancel: &CancelToken,
) -> Result<()> {
    let start = start.unwrap_or(Revnum(0));
    let end = peg(wire, end, cancel).await?;
    if start > end && !wire.caps().dav.reverse_file_revs {
        return Err(SvnError::new(
            ErrorCode::ServerUnsupported,
            "the server cannot report file revisions in reverse order",
        ));
    }
    // libsvn_ra_serf addresses the newer end of the range.
    let peg_rev = start.max(end);
    let repo = wire.repo_path(path)?;
    let mut drive = Drive {
        handler,
        pending: None,
        sink: None,
        in_delta: false,
        delivered: 0,
        skip: 0,
    };
    // The report is the same every time it is asked for (same peg, same range), each delta
    // against the previous revision of the report. One that breaks off between two revisions is
    // asked for again, and the revisions the handler already has are skipped; one that breaks
    // off inside a delta the handler is receiving cannot be resumed.
    let mut retry = ReadRetry::new(wire, "file history");
    loop {
        let req = file_revs_request(wire.urls(), peg_rev, &repo, start, end, false)?;
        let before = drive.delivered;
        drive.skip = drive.delivered;
        drive.pending = None;
        drive.in_delta = false;
        let mut handler_failed = false;
        let attempt = async {
            let resp = wire.send_ok(req, cancel).await?;
            let mut parser = FileRevsParser::new();
            stream_items(resp, &mut parser, cancel, |ev| {
                drive.handle(ev).inspect_err(|_| handler_failed = true)
            })
            .await
        }
        .await;
        let err = match attempt {
            Ok(()) => break,
            Err(e) if handler_failed => return Err(e),
            Err(e) => e,
        };
        if drive.sink.take().is_some() {
            // Part of a delta reached the handler's sink.
            return Err(if err.connection_failure().is_some() {
                cannot_resume(err, "file history")
            } else {
                err
            });
        }
        if drive.delivered > before {
            retry.progressed();
        }
        retry.again(err, cancel).await?;
    }
    if drive.in_delta || drive.sink.is_some() {
        return Err(corrupt("file-revs report ended inside a delta"));
    }
    Ok(())
}

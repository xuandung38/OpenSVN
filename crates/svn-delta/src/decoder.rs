//! Streaming svndiff decoder: accepts the stream in arbitrary chunks, however split.
//!
//! Cost model: every input byte is copied at most once into the pending buffer, the window header
//! is parsed once and cached, and consumed bytes are dropped lazily (when at least half the buffer
//! is garbage), so total work is linear in the stream size whatever the chunk size.

use bytes::Bytes;
use svn_types::{ErrorCode, Result, SvnError, TxDeltaWindow, WindowSink};

use crate::compress::decode_section;
use crate::encoder::SvndiffVersion;
use crate::varint::decode_varint;
use crate::window::{MAX_INSTRUCTIONS_LEN, MAX_VIEW_LEN, parse_instructions};

/// Stored sections may carry a varint length prefix on top of the content limits.
const SECTION_SLACK: usize = 16;

fn corrupt(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::Corrupt, msg)
}

/// Header of the window being decoded: five integers.
#[derive(Debug, Clone, Copy)]
struct WindowHeader {
    sview_offset: u64,
    sview_len: u64,
    tview_len: u64,
    ins_len: usize,
    new_len: usize,
}

/// Parsing state that does not borrow the input buffer.
#[derive(Debug, Default)]
struct Core {
    version: Option<SvndiffVersion>,
    /// Header parsed but its body is not fully buffered yet.
    header: Option<WindowHeader>,
    /// Source view of the previous window, for the monotonic-view rule.
    last_view: Option<(u64, u64)>,
}

/// Incremental decoder for svndiff 0, 1 and 2.
///
/// At most one window (plus a little slack) is buffered. After the first error the decoder is
/// poisoned and keeps failing.
#[derive(Debug, Default)]
pub struct SvndiffDecoder {
    core: Core,
    /// Unconsumed input is `buf[pos..]`.
    buf: Vec<u8>,
    pos: usize,
    failed: bool,
}

impl SvndiffDecoder {
    /// Creates a decoder positioned at the start of a stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds the next chunk and returns every window completed by it.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<TxDeltaWindow>> {
        if self.failed {
            return Err(corrupt("svndiff decoder already failed"));
        }
        let mut out = Vec::new();
        let result = self.feed_inner(chunk, &mut out);
        match result {
            Ok(()) => Ok(out),
            Err(e) => {
                self.failed = true;
                self.buf = Vec::new();
                self.pos = 0;
                Err(e)
            }
        }
    }

    /// Declares end of input. A stream without even a header, or one that stops inside the header
    /// or a window, is `Corrupt` (as in Subversion; an empty delta is the 4-byte header alone).
    pub fn finish(&mut self) -> Result<()> {
        if self.failed {
            return Err(corrupt("svndiff decoder already failed"));
        }
        let complete =
            self.core.version.is_some() && self.core.header.is_none() && self.pos == self.buf.len();
        if complete {
            Ok(())
        } else {
            self.failed = true;
            Err(corrupt(
                "svndiff stream is empty or ends inside a header or window",
            ))
        }
    }

    /// [`feed`](Self::feed) that forwards each window to `sink`.
    pub fn feed_sink(&mut self, chunk: &[u8], sink: &mut dyn WindowSink) -> Result<()> {
        for w in self.feed(chunk)? {
            sink.window(Some(&w))?;
        }
        Ok(())
    }

    /// [`finish`](Self::finish) followed by the end-of-delta marker (`None`) to `sink`.
    pub fn finish_sink(&mut self, sink: &mut dyn WindowSink) -> Result<()> {
        self.finish()?;
        sink.window(None)
    }

    fn feed_inner(&mut self, chunk: &[u8], out: &mut Vec<TxDeltaWindow>) -> Result<()> {
        if self.pos == self.buf.len() {
            // Nothing carried over: parse straight from the caller's slice, keep only the tail.
            self.buf.clear();
            self.pos = 0;
            let used = self.core.run(chunk, out)?;
            self.buf.extend_from_slice(&chunk[used..]);
            return Ok(());
        }
        self.buf.extend_from_slice(chunk);
        self.pos += self.core.run(&self.buf[self.pos..], out)?;
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        } else if self.pos >= self.buf.len() / 2 {
            // Consumed part is at least as large as the live tail: compacting is amortised O(1).
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        Ok(())
    }
}

impl Core {
    /// Consumes as much of `data` as forms complete units; returns the number of bytes used.
    fn run(&mut self, data: &[u8], out: &mut Vec<TxDeltaWindow>) -> Result<usize> {
        let mut pos = 0;
        let version = match self.version {
            Some(v) => v,
            None => {
                let head = &data[..data.len().min(3)];
                if !b"SVN".starts_with(head) {
                    return Err(corrupt("svndiff stream has a bad magic"));
                }
                if data.len() < 4 {
                    return Ok(0);
                }
                let v = SvndiffVersion::from_byte(data[3])
                    .ok_or_else(|| corrupt(format!("unsupported svndiff version {}", data[3])))?;
                self.version = Some(v);
                pos = 4;
                v
            }
        };
        loop {
            let header = match self.header {
                Some(h) => h,
                None => match Self::parse_header(&data[pos..])? {
                    Some((h, used)) => {
                        pos += used;
                        self.header = Some(h);
                        h
                    }
                    None => break,
                },
            };
            let body = header.ins_len + header.new_len;
            if data.len() - pos < body {
                break;
            }
            let ins_end = pos + header.ins_len;
            let window = self.build_window(
                version,
                &header,
                &data[pos..ins_end],
                &data[ins_end..pos + body],
            )?;
            out.push(window);
            self.header = None;
            pos += body;
        }
        Ok(pos)
    }

    /// Parses the five window integers; `None` when more bytes are needed.
    fn parse_header(buf: &[u8]) -> Result<Option<(WindowHeader, usize)>> {
        let mut vals = [0u64; 5];
        let mut used = 0;
        for slot in &mut vals {
            match decode_varint(&buf[used..])? {
                Some((v, n)) => {
                    *slot = v;
                    used += n;
                }
                None => return Ok(None),
            }
        }
        let [sview_offset, sview_len, tview_len, ins_len, new_len] = vals;
        let ins_len = usize::try_from(ins_len)
            .ok()
            .filter(|n| *n <= MAX_INSTRUCTIONS_LEN + SECTION_SLACK)
            .ok_or_else(|| corrupt("svndiff instruction section is too large"))?;
        let new_len = usize::try_from(new_len)
            .ok()
            .filter(|n| *n <= MAX_VIEW_LEN + SECTION_SLACK)
            .ok_or_else(|| corrupt("svndiff new-data section is too large"))?;
        if sview_len > MAX_VIEW_LEN as u64 || tview_len > MAX_VIEW_LEN as u64 {
            return Err(corrupt("svndiff window view is too large"));
        }
        if sview_offset.checked_add(sview_len).is_none() {
            return Err(corrupt("svndiff source view overflows"));
        }
        Ok(Some((
            WindowHeader {
                sview_offset,
                sview_len,
                tview_len,
                ins_len,
                new_len,
            },
            used,
        )))
    }

    fn build_window(
        &mut self,
        version: SvndiffVersion,
        h: &WindowHeader,
        ins_stored: &[u8],
        new_stored: &[u8],
    ) -> Result<TxDeltaWindow> {
        // Source views may only move forward, as in Subversion's decoder. Window headers were
        // checked against overflow when parsed.
        if h.sview_len > 0
            && let Some((last_off, last_len)) = self.last_view
            && (h.sview_offset < last_off || h.sview_offset + h.sview_len < last_off + last_len)
        {
            return Err(corrupt("svndiff source views must not go backwards"));
        }
        let ins = decode_section(version, ins_stored, MAX_INSTRUCTIONS_LEN)?;
        let new_data = decode_section(version, new_stored, MAX_VIEW_LEN)?;
        let ops = parse_instructions(&ins, h.sview_len, h.tview_len, new_data.len() as u64)?;
        self.last_view = Some((h.sview_offset, h.sview_len));
        Ok(TxDeltaWindow {
            sview_offset: h.sview_offset,
            sview_len: h.sview_len,
            tview_len: h.tview_len,
            ops,
            new_data: Bytes::from(new_data.into_owned()),
        })
    }
}

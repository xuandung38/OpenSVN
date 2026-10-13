//! Streaming svndiff decoder: accepts the stream in arbitrary chunks, however split.

use bytes::Bytes;
use svn_types::{ErrorCode, Result, SvnError, TxDeltaWindow, WindowSink};

use crate::compress::decode_section;
use crate::encoder::SvndiffVersion;
use crate::varint::decode_varint;
use crate::window::{MAX_INSTRUCTIONS_LEN, MAX_VIEW_LEN, parse_instructions};

fn corrupt(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::Corrupt, msg)
}

/// Incremental decoder for svndiff 0, 1 and 2.
///
/// Only the unfinished tail of the stream (at most one window) is buffered. After the first error
/// the decoder is poisoned and keeps failing.
#[derive(Debug, Default)]
pub struct SvndiffDecoder {
    version: Option<SvndiffVersion>,
    /// Unconsumed bytes: a partial header or a partial window.
    pending: Vec<u8>,
    /// Source view of the previous window, for the monotonic-view rule.
    last_view: Option<(u64, u64)>,
    failed: bool,
}

/// Header of the window being decoded: five integers.
struct WindowHeader {
    sview_offset: u64,
    sview_len: u64,
    tview_len: u64,
    ins_len: usize,
    new_len: usize,
    /// Bytes taken by the five integers.
    header_len: usize,
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
        let result = self.feed_inner(chunk);
        if result.is_err() {
            self.failed = true;
            self.pending = Vec::new();
        }
        result
    }

    /// Declares end of input. An empty stream is a valid empty delta; a stream that stops inside
    /// the header or a window is `Corrupt`.
    pub fn finish(&mut self) -> Result<()> {
        if self.failed {
            return Err(corrupt("svndiff decoder already failed"));
        }
        if !self.pending.is_empty() {
            self.failed = true;
            return Err(corrupt("svndiff stream ends inside a header or window"));
        }
        Ok(())
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

    fn feed_inner(&mut self, chunk: &[u8]) -> Result<Vec<TxDeltaWindow>> {
        let carried = std::mem::take(&mut self.pending);
        let joined;
        let data: &[u8] = if carried.is_empty() {
            chunk
        } else {
            joined = [carried.as_slice(), chunk].concat();
            &joined
        };
        let mut pos = 0;
        let mut out = Vec::new();

        let version = match self.version {
            Some(v) => v,
            None => {
                if data.len() < 4 {
                    if !b"SVN".starts_with(&data[..data.len().min(3)]) {
                        return Err(corrupt("svndiff stream has a bad magic"));
                    }
                    self.pending = data.to_vec();
                    return Ok(out);
                }
                if &data[..3] != b"SVN" {
                    return Err(corrupt("svndiff stream has a bad magic"));
                }
                let v = SvndiffVersion::from_byte(data[3])
                    .ok_or_else(|| corrupt(format!("unsupported svndiff version {}", data[3])))?;
                self.version = Some(v);
                pos = 4;
                v
            }
        };

        while pos < data.len() {
            let rest = &data[pos..];
            let Some(header) = Self::parse_header(rest)? else {
                break;
            };
            let total = header
                .header_len
                .checked_add(header.ins_len)
                .and_then(|n| n.checked_add(header.new_len))
                .ok_or_else(|| corrupt("svndiff window length overflows"))?;
            if rest.len() < total {
                break;
            }
            let new_start = header.header_len + header.ins_len;
            let window = self.build_window(
                version,
                &header,
                &rest[header.header_len..new_start],
                &rest[new_start..total],
            )?;
            out.push(window);
            pos += total;
        }
        self.pending = data[pos..].to_vec();
        Ok(out)
    }

    /// Parses the five window integers; `None` when more bytes are needed.
    fn parse_header(buf: &[u8]) -> Result<Option<WindowHeader>> {
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
        // Stored sections may carry a varint length prefix on top of the content limits.
        let ins_len = usize::try_from(ins_len)
            .ok()
            .filter(|n| *n <= MAX_INSTRUCTIONS_LEN + 16)
            .ok_or_else(|| corrupt("svndiff instruction section is too large"))?;
        let new_len = usize::try_from(new_len)
            .ok()
            .filter(|n| *n <= MAX_VIEW_LEN + 16)
            .ok_or_else(|| corrupt("svndiff new-data section is too large"))?;
        if sview_len > MAX_VIEW_LEN as u64 || tview_len > MAX_VIEW_LEN as u64 {
            return Err(corrupt("svndiff window view is too large"));
        }
        if sview_offset.checked_add(sview_len).is_none() {
            return Err(corrupt("svndiff source view overflows"));
        }
        Ok(Some(WindowHeader {
            sview_offset,
            sview_len,
            tview_len,
            ins_len,
            new_len,
            header_len: used,
        }))
    }

    fn build_window(
        &mut self,
        version: SvndiffVersion,
        h: &WindowHeader,
        ins_stored: &[u8],
        new_stored: &[u8],
    ) -> Result<TxDeltaWindow> {
        // Source views may only move forward, as in Subversion's decoder. Both sums were checked
        // against overflow when the header was parsed (and last_view comes from a parsed header).
        if let Some((last_off, last_len)) = self.last_view
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

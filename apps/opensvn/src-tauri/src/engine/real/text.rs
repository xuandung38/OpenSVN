//! Text helpers of the real engine: percent decoding, text sides, size-capped buffers and line
//! attribution for blame.

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use opensvn_ipc::{Eol, TextSide};
use tokio::io::AsyncWrite;

/// Decodes `%XX` escapes (invalid escapes stay literal).
pub(super) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Detects the line ending style of `text`.
fn detect_eol(text: &str) -> Eol {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count();
    match (crlf, lf - crlf) {
        (0, _) => Eol::Lf,
        (_, 0) => Eol::Crlf,
        _ => Eol::Mixed,
    }
}

/// Whether `data` should be treated as binary (NUL byte or invalid UTF-8).
pub(super) fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|b| *b == 0) || std::str::from_utf8(data).is_err()
}

/// Text side for UTF-8 content (invalid sequences are replaced).
pub(super) fn text_side(data: &[u8], label: String) -> TextSide {
    let content = String::from_utf8_lossy(data).into_owned();
    TextSide {
        eol: detect_eol(&content),
        content,
        encoding: "UTF-8".to_owned(),
        label,
    }
}

/// For each line of `new`, the index of the matching line in `old` (longest common subsequence).
pub(super) fn match_lines(old: &[&str], new: &[&str]) -> Vec<Option<usize>> {
    let (n, m) = (old.len(), new.len());
    let mut table = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if old[i] == new[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut out = vec![None; m];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old[i] == new[j] {
            out[j] = Some(i);
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// In-memory sink that refuses to grow beyond a limit, so a huge file is never buffered whole.
pub(super) struct CappedBuf {
    buf: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl CappedBuf {
    /// Creates a sink holding at most `limit` bytes.
    pub(super) fn new(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            limit,
            exceeded: false,
        }
    }

    /// Whether a write was refused because of the limit.
    pub(super) fn exceeded(&self) -> bool {
        self.exceeded
    }

    /// The collected bytes.
    pub(super) fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

impl AsyncWrite for CappedBuf {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.buf.len().saturating_add(data.len()) > self.limit {
            self.exceeded = true;
            return Poll::Ready(Err(std::io::Error::other("size limit exceeded")));
        }
        self.buf.extend_from_slice(data);
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Cloneable synchronous byte sink; lets the caller read what a boxed delta applier produced.
#[derive(Clone, Default)]
pub(super) struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    /// Takes the collected bytes, leaving the buffer empty.
    pub(super) fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl std::io::Write for SharedBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

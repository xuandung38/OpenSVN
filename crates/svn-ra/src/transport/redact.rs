//! Redaction of secrets in log output.
//!
//! Two layers protect the logs. Transport code only logs headers and URLs through
//! [`RedactedHeaders`] and [`redacted_url`]. As defense in depth, [`RedactingMakeWriter`] wraps
//! any `tracing-subscriber` writer and scrubs the formatted text with [`redact_text`], so a
//! secret that reaches a log line by another route is still removed.

use std::borrow::Cow;
use std::fmt;
use std::io::{self, Write};

use reqwest::header::HeaderMap;
use tracing_subscriber::fmt::MakeWriter;
use url::Url;

/// Header names whose values must never be logged (lower case).
pub const SENSITIVE_HEADERS: [&str; 6] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "lock-token",
    "if",
];

/// Marker substituted for removed secrets.
const MASK: &str = "***";

/// Whether `name` is a sensitive header (case-insensitive).
pub fn is_sensitive_header(name: &str) -> bool {
    SENSITIVE_HEADERS
        .iter()
        .any(|h| h.eq_ignore_ascii_case(name))
}

/// Display adapter that prints headers with sensitive values masked.
pub struct RedactedHeaders<'a>(pub &'a HeaderMap);

impl fmt::Display for RedactedHeaders<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, (name, value)) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            if is_sensitive_header(name.as_str()) || value.is_sensitive() {
                write!(f, "{name}: {MASK}")?;
            } else {
                write!(f, "{name}: {}", value.to_str().unwrap_or("<binary>"))?;
            }
        }
        Ok(())
    }
}

/// URL text with userinfo removed.
pub fn redacted_url(url: &Url) -> String {
    let mut u = url.clone();
    // Both setters only fail for cannot-be-a-base URLs, which carry no userinfo.
    let _ = u.set_username("");
    let _ = u.set_password(None);
    u.to_string()
}

/// Masks the userinfo of every `scheme://user:pass@host` in `text`.
fn redact_userinfo(text: &str) -> Cow<'_, str> {
    if !text.contains("://") {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("://") {
        let (head, tail) = rest.split_at(pos + 3);
        out.push_str(head);
        // The authority ends at the first delimiter that cannot appear in userinfo.
        let end = tail
            .find(|c: char| {
                matches!(c, '/' | '?' | '#' | '"' | '\'' | '<' | '>') || c.is_whitespace()
            })
            .unwrap_or(tail.len());
        let (authority, after) = tail.split_at(end);
        match authority.rfind('@') {
            Some(at) => {
                out.push_str(MASK);
                out.push_str(&authority[at..]);
            }
            None => out.push_str(authority),
        }
        rest = after;
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Removes sensitive header values from free text such as `Authorization: Basic abc`,
/// `authorization="Basic abc"` or a debug-printed header map.
fn redact_headers_in_text(text: &str) -> Cow<'_, str> {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut copied = 0usize;
    let mut changed = false;
    let mut i = 0usize;
    while i < lower.len() {
        let Some(name) = SENSITIVE_HEADERS
            .iter()
            .find(|n| lower.as_bytes()[i..].starts_with(n.as_bytes()))
        else {
            i += 1;
            continue;
        };
        let name_end = i + name.len();
        let boundary_before = i == 0 || !is_name_char(bytes[i - 1]);
        // Optional closing quote, spaces, then a separator.
        let mut j = name_end;
        if matches!(bytes.get(j), Some(b'"' | b'\'')) {
            j += 1;
        }
        while matches!(bytes.get(j), Some(b' ' | b'\t')) {
            j += 1;
        }
        if !boundary_before || !matches!(bytes.get(j), Some(b':' | b'=')) {
            i += 1;
            continue;
        }
        j += 1;
        while matches!(bytes.get(j), Some(b' ' | b'\t')) {
            j += 1;
        }
        // Value: up to the matching quote when quoted, else to the end of the line.
        let (value_start, value_end) = match bytes.get(j) {
            Some(&q @ (b'"' | b'\'')) => {
                let mut k = j + 1;
                while k < bytes.len() && bytes[k] != q {
                    k += if bytes[k] == b'\\' { 2 } else { 1 };
                }
                (j + 1, k.min(bytes.len()))
            }
            _ => {
                let k = text[j..].find(['\n', '\r']).map_or(text.len(), |p| j + p);
                (j, k)
            }
        };
        out.push_str(&text[copied..value_start]);
        out.push_str(MASK);
        copied = value_end;
        changed = true;
        i = value_end.max(i + 1);
    }
    if !changed {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

fn is_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Scrubs URL userinfo and sensitive header values from `text`.
pub fn redact_text(text: &str) -> Cow<'_, str> {
    match redact_userinfo(text) {
        Cow::Borrowed(t) => redact_headers_in_text(t),
        Cow::Owned(s) => Cow::Owned(redact_headers_in_text(&s).into_owned()),
    }
}

/// Writer that buffers one log record and redacts it when flushed or dropped.
pub struct RedactingWriter<W: Write> {
    buf: Vec<u8>,
    inner: W,
}

impl<W: Write> RedactingWriter<W> {
    /// Wraps `inner`.
    pub fn new(inner: W) -> Self {
        Self {
            buf: Vec::new(),
            inner,
        }
    }

    fn emit(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&self.buf);
        let redacted = redact_text(&text);
        let res = self.inner.write_all(redacted.as_bytes());
        self.buf.clear();
        res
    }
}

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.emit()?;
        self.inner.flush()
    }
}

impl<W: Write> Drop for RedactingWriter<W> {
    fn drop(&mut self) {
        // Errors cannot be reported from drop; a failed log write is not actionable.
        let _ = self.emit();
    }
}

/// [`MakeWriter`] adapter: install with
/// `tracing_subscriber::fmt().with_writer(RedactingMakeWriter::new(io::stderr))`.
#[derive(Clone)]
pub struct RedactingMakeWriter<M>(M);

impl<M> RedactingMakeWriter<M> {
    /// Wraps a writer factory.
    pub fn new(make: M) -> Self {
        Self(make)
    }
}

impl<'a, M> MakeWriter<'a> for RedactingMakeWriter<M>
where
    M: MakeWriter<'a>,
{
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter::new(self.0.make_writer())
    }
}

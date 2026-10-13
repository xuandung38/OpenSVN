//! `file-revs-report` (blame): request builder and streaming event parser.
//!
//! The server sends, for every revision that changed the file, its revision properties, property
//! changes and a text delta against the previous version. The delta stays base64-decoded svndiff
//! bytes; applying it is the caller's business.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use svn_types::{RelPath, Result, Revnum};

use crate::caps::{UrlBuilder, advertise_caps};
use crate::display_path::RepoDisplayPath;
use crate::http::{HttpRequest, Method, ResponseParser, parse_chunked};
use crate::limits::ParserLimits;
use crate::util::{Base64Stream, corrupt, parse_revnum};
use crate::xml::{Ns, StartTag, XmlEvent, XmlReader, XmlWriter};

/// Builds `REPORT rvr/PEG/file` with an `S:file-revs-report` body for revisions `start..=end`.
pub fn file_revs_request(
    urls: &UrlBuilder,
    peg: Revnum,
    file: &RelPath,
    start: Revnum,
    end: Revnum,
    include_merged_revisions: bool,
) -> Result<HttpRequest> {
    let mut w = XmlWriter::new();
    w.start("S:file-revs-report", &[("xmlns:S", "svn:")])
        .element("S:start-revision", &[], &start.0.to_string())
        .element("S:end-revision", &[], &end.0.to_string());
    if include_merged_revisions {
        w.empty("S:include-merged-revisions", &[]);
    }
    w.element("S:path", &[], "").end();
    let req = HttpRequest::new(Method::Report, urls.rev_root(peg, file)?).xml_body(w.finish()?);
    Ok(advertise_caps(req))
}

/// Revision range to request and whether the caller must reverse the answer itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRevsPlan {
    /// `S:start-revision` to send.
    pub start: Revnum,
    /// `S:end-revision` to send.
    pub end: Revnum,
    /// The server answers oldest first only; reverse the revisions after receiving them.
    pub client_reverses: bool,
}

/// Capability-driven range choice: a descending range (`start > end`) is sent as is when the
/// server has `reverse-file-revs`; otherwise it is asked for ascending and flagged for reversal.
pub fn plan_file_revs(caps: &crate::Capabilities, start: Revnum, end: Revnum) -> FileRevsPlan {
    if start > end && !caps.dav.reverse_file_revs {
        FileRevsPlan {
            start: end,
            end: start,
            client_reverses: true,
        }
    } else {
        FileRevsPlan {
            start,
            end,
            client_reverses: false,
        }
    }
}

/// One step of a file-revs answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRevEvent {
    /// A revision of the file begins.
    Start {
        /// Location of the file in that revision (display only).
        path: RepoDisplayPath,
        /// Revision.
        rev: Revnum,
    },
    /// The revision was merged in from elsewhere (`S:merged-revision`).
    MergedRevision,
    /// Revision property (`svn:log`, `svn:author`, ...).
    RevProp {
        /// Name.
        name: String,
        /// Decoded value.
        value: Bytes,
    },
    /// File property set.
    SetProp {
        /// Name.
        name: String,
        /// Decoded value.
        value: Bytes,
    },
    /// File property removed.
    RemoveProp {
        /// Name.
        name: String,
    },
    /// The svndiff stream against the previous version starts.
    TxDeltaStart,
    /// Next svndiff bytes (base64-decoded).
    TxDeltaChunk(Bytes),
    /// The svndiff stream ended.
    TxDeltaEnd,
    /// The revision is complete.
    End,
}

enum Capture {
    None,
    Prop {
        kind: PropKind,
        name: String,
        base64: bool,
        text: String,
    },
    TxDelta(Base64Stream),
}

#[derive(Clone, Copy)]
enum PropKind {
    Rev,
    Set,
}

/// Incremental parser for `S:file-revs-report`.
pub struct FileRevsParser {
    xml: XmlReader,
    depth: usize,
    capture: Capture,
    capture_depth: usize,
    in_rev: bool,
    limits: ParserLimits,
    failed: bool,
}

impl Default for FileRevsParser {
    fn default() -> Self {
        Self::new()
    }
}

fn prop_name(tag: &StartTag, what: &str) -> Result<String> {
    tag.attr("name")
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| corrupt(format!("{what} without a name")))
}

impl FileRevsParser {
    /// Creates a parser.
    pub fn new() -> Self {
        Self {
            xml: XmlReader::new(),
            depth: 0,
            capture: Capture::None,
            capture_depth: 0,
            in_rev: false,
            limits: ParserLimits::default(),
            failed: false,
        }
    }

    /// Creates a parser with explicit limits.
    pub fn with_limits(limits: ParserLimits) -> Self {
        Self {
            limits,
            ..Self::new()
        }
    }

    fn start(&mut self, tag: &StartTag, out: &mut Vec<FileRevEvent>) -> Result<()> {
        self.depth += 1;
        if self.depth == 1 {
            return if tag.name.is(&Ns::Svn, "file-revs-report") {
                Ok(())
            } else {
                Err(corrupt("expected an S:file-revs-report document"))
            };
        }
        if !matches!(self.capture, Capture::None) || tag.name.ns != Ns::Svn {
            return Ok(());
        }
        match (self.depth, tag.name.local.as_str()) {
            (2, "file-rev") => {
                let path = tag
                    .attr("path")
                    .ok_or_else(|| corrupt("file-rev without a path"))?;
                let rev = tag
                    .attr("rev")
                    .ok_or_else(|| corrupt("file-rev without a revision"))?;
                self.in_rev = true;
                out.push(FileRevEvent::Start {
                    path: RepoDisplayPath::parse(path)?,
                    rev: parse_revnum(rev)?,
                });
            }
            (3, "merged-revision") if self.in_rev => out.push(FileRevEvent::MergedRevision),
            (3, "remove-prop") if self.in_rev => out.push(FileRevEvent::RemoveProp {
                name: prop_name(tag, "remove-prop")?,
            }),
            (3, kind @ ("rev-prop" | "set-prop")) if self.in_rev => {
                self.capture = Capture::Prop {
                    kind: if kind == "rev-prop" {
                        PropKind::Rev
                    } else {
                        PropKind::Set
                    },
                    name: prop_name(tag, kind)?,
                    base64: tag.attr("encoding") == Some("base64"),
                    text: String::new(),
                };
            }
            (3, "txdelta") if self.in_rev => {
                out.push(FileRevEvent::TxDeltaStart);
                self.capture = Capture::TxDelta(Base64Stream::default());
            }
            _ => {}
        }
        Ok(())
    }

    fn text(&mut self, text: &str, out: &mut Vec<FileRevEvent>) -> Result<()> {
        match &mut self.capture {
            Capture::None => {}
            Capture::Prop { text: buf, .. } => {
                buf.push_str(text);
                ParserLimits::check(buf.len(), self.limits.max_text_bytes, "property value")?;
            }
            Capture::TxDelta(b64) => {
                let data = b64.push(text)?;
                if !data.is_empty() {
                    out.push(FileRevEvent::TxDeltaChunk(Bytes::from(data)));
                }
            }
        }
        Ok(())
    }

    fn end(&mut self, out: &mut Vec<FileRevEvent>) -> Result<()> {
        let was = self.depth;
        self.depth = self.depth.saturating_sub(1);
        if !matches!(self.capture, Capture::None) {
            if was != self.capture_depth {
                return Ok(());
            }
            match std::mem::replace(&mut self.capture, Capture::None) {
                Capture::None => {}
                Capture::Prop {
                    kind,
                    name,
                    base64,
                    text,
                } => {
                    let value = if base64 {
                        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
                        STANDARD
                            .decode(compact.as_bytes())
                            .map_err(|e| corrupt(format!("bad base64 in property {name}: {e}")))?
                    } else {
                        text.into_bytes()
                    };
                    let value = Bytes::from(value);
                    out.push(match kind {
                        PropKind::Rev => FileRevEvent::RevProp { name, value },
                        PropKind::Set => FileRevEvent::SetProp { name, value },
                    });
                }
                Capture::TxDelta(mut b64) => {
                    b64.finish()?;
                    out.push(FileRevEvent::TxDeltaEnd);
                }
            }
            return Ok(());
        }
        if was == 2 && self.in_rev {
            self.in_rev = false;
            out.push(FileRevEvent::End);
        }
        Ok(())
    }

    fn run(&mut self, events: Vec<XmlEvent>) -> Result<Vec<FileRevEvent>> {
        let mut out = Vec::new();
        for ev in events {
            match ev {
                XmlEvent::Start(tag) => {
                    self.start(&tag, &mut out)?;
                    if !matches!(self.capture, Capture::None) && self.capture_depth == 0 {
                        self.capture_depth = self.depth;
                    }
                }
                XmlEvent::Text(t) => self.text(&t, &mut out)?,
                XmlEvent::End(_) => {
                    self.end(&mut out)?;
                    if matches!(self.capture, Capture::None) {
                        self.capture_depth = 0;
                    }
                }
            }
        }
        Ok(out)
    }
}

impl ResponseParser for FileRevsParser {
    type Item = FileRevEvent;

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<FileRevEvent>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.feed(chunk).and_then(|events| self.run(events));
        self.failed = result.is_err();
        result
    }

    fn finish(&mut self) -> Result<Vec<FileRevEvent>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.finish().and_then(|events| self.run(events));
        self.failed = result.is_err();
        result
    }
}

/// Parses a complete file-revs-report body.
pub fn parse_file_revs(body: &[u8]) -> Result<Vec<FileRevEvent>> {
    parse_chunked(&mut FileRevsParser::new(), body, 0)
}

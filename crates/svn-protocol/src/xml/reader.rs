//! Push-style, namespace-aware streaming XML reader.
//!
//! Bytes arrive in arbitrary chunks. A byte-level scanner finds the longest prefix that ends on
//! a markup boundary (or, for long text runs, on a character boundary outside an entity); that
//! prefix is handed to `quick-xml` for tokenizing and the events are namespace-resolved here.
//! Memory use is bounded by [`XmlLimits`], never by the document size. DTDs are rejected, so no
//! entity can expand beyond the five predefined ones and character references.

use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::Event;
use svn_types::{Result, SvnError};

use super::ns::{Name, Ns};
use crate::util::corrupt;

/// Resource limits that keep hostile documents from exhausting memory.
#[derive(Debug, Clone, Copy)]
pub struct XmlLimits {
    /// Longest single tag, comment, CDATA section or processing instruction.
    pub max_markup: usize,
    /// Deepest element nesting.
    pub max_depth: usize,
    /// Most attributes (including namespace declarations) on one element.
    pub max_attrs: usize,
    /// Text runs longer than this are delivered in several [`XmlEvent::Text`] pieces.
    pub text_flush: usize,
}

impl Default for XmlLimits {
    fn default() -> Self {
        Self {
            max_markup: 1 << 20,
            max_depth: 128,
            max_attrs: 128,
            text_flush: 8 * 1024,
        }
    }
}

/// One attribute with a resolved name and unescaped value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    /// Qualified name (unprefixed attributes have [`Ns::None`]).
    pub name: Name,
    /// Unescaped value.
    pub value: String,
}

/// Start tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartTag {
    /// Element name.
    pub name: Name,
    /// Attributes without namespace declarations.
    pub attrs: Vec<Attr>,
}

impl StartTag {
    /// Value of the first attribute whose local name is `local`, whatever its namespace.
    pub fn attr(&self, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.name.local == local)
            .map(|a| a.value.as_str())
    }
}

/// Event produced by [`XmlReader`]. Empty elements yield `Start` followed by `End`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XmlEvent {
    /// Element opened.
    Start(StartTag),
    /// Element closed.
    End(Name),
    /// Unescaped character data. One logical text node may arrive in several pieces.
    Text(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scan {
    Text,
    Lt,
    Tag(u8),
    Bang,
    BangDash,
    CdataHead(u8),
    CdataBody(u8),
    Comment(u8),
    Pi(bool),
}

struct Scope {
    lexical: String,
    decls: Vec<(String, String)>,
}

/// Streaming namespace-aware XML reader. See the module documentation.
pub struct XmlReader {
    limits: XmlLimits,
    buf: Vec<u8>,
    scan: Scan,
    scan_pos: usize,
    safe_end: usize,
    amp: Option<usize>,
    stack: Vec<Scope>,
    saw_root: bool,
    root_closed: bool,
    first_chunk: bool,
    failed: bool,
}

impl Default for XmlReader {
    fn default() -> Self {
        Self::new()
    }
}

fn xml_err(e: impl std::fmt::Display) -> SvnError {
    corrupt(format!("malformed XML: {e}"))
}

impl XmlReader {
    /// Creates a reader with default limits.
    pub fn new() -> Self {
        Self::with_limits(XmlLimits::default())
    }

    /// Creates a reader with explicit limits.
    pub fn with_limits(limits: XmlLimits) -> Self {
        Self {
            limits,
            buf: Vec::new(),
            scan: Scan::Text,
            scan_pos: 0,
            safe_end: 0,
            amp: None,
            stack: Vec::new(),
            saw_root: false,
            root_closed: false,
            first_chunk: true,
            failed: false,
        }
    }

    /// Feeds the next chunk and returns every event that is complete.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<XmlEvent>> {
        if self.failed {
            return Err(corrupt("XML reader used after an error"));
        }
        self.buf.extend_from_slice(chunk);
        let result = self.drain();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// Signals end of input; fails if the document is truncated or has no root element.
    pub fn finish(&mut self) -> Result<Vec<XmlEvent>> {
        if self.failed {
            return Err(corrupt("XML reader used after an error"));
        }
        let mut events = Vec::new();
        if self.scan != Scan::Text {
            self.failed = true;
            return Err(corrupt("malformed XML: truncated markup"));
        }
        // Whatever text is left lies outside the root element.
        let end = self.buf.len();
        self.safe_end = end;
        if let Err(e) = self.process(end, &mut events) {
            self.failed = true;
            return Err(e);
        }
        if !self.stack.is_empty() || !self.saw_root {
            self.failed = true;
            return Err(corrupt("malformed XML: document is truncated or empty"));
        }
        Ok(events)
    }

    fn drain(&mut self) -> Result<Vec<XmlEvent>> {
        self.advance_scanner()?;
        self.maybe_flush_text();
        let mut events = Vec::new();
        let end = self.safe_end;
        if end > 0 {
            self.process(end, &mut events)?;
        }
        Ok(events)
    }

    fn advance_scanner(&mut self) -> Result<()> {
        let max = self.limits.max_markup;
        let mut i = self.scan_pos;
        while i < self.buf.len() {
            let b = self.buf[i];
            self.scan = match self.scan {
                Scan::Text => match b {
                    b'<' => {
                        self.safe_end = i;
                        self.amp = None;
                        Scan::Lt
                    }
                    b'&' => {
                        self.amp = Some(i);
                        Scan::Text
                    }
                    b';' => {
                        self.amp = None;
                        Scan::Text
                    }
                    _ => Scan::Text,
                },
                Scan::Lt => match b {
                    b'!' => Scan::Bang,
                    b'?' => Scan::Pi(false),
                    _ => Scan::Tag(0),
                },
                Scan::Tag(0) => match b {
                    b'"' | b'\'' => Scan::Tag(b),
                    b'>' => {
                        self.safe_end = i + 1;
                        Scan::Text
                    }
                    b'<' => return Err(corrupt("malformed XML: '<' inside a tag")),
                    _ => Scan::Tag(0),
                },
                Scan::Tag(q) => {
                    if b == q {
                        Scan::Tag(0)
                    } else {
                        Scan::Tag(q)
                    }
                }
                Scan::Bang => match b {
                    b'-' => Scan::BangDash,
                    b'[' => Scan::CdataHead(1),
                    _ => return Err(corrupt("DTD and other declarations are not allowed")),
                },
                Scan::BangDash => match b {
                    b'-' => Scan::Comment(0),
                    _ => return Err(corrupt("malformed XML: bad comment start")),
                },
                Scan::CdataHead(n) => {
                    if b"[CDATA[".get(usize::from(n)) == Some(&b) {
                        if n == 6 {
                            Scan::CdataBody(0)
                        } else {
                            Scan::CdataHead(n + 1)
                        }
                    } else {
                        return Err(corrupt("DTD and other declarations are not allowed"));
                    }
                }
                Scan::CdataBody(k) => match b {
                    b']' => Scan::CdataBody((k + 1).min(2)),
                    b'>' if k >= 2 => {
                        self.safe_end = i + 1;
                        Scan::Text
                    }
                    _ => Scan::CdataBody(0),
                },
                Scan::Comment(d) => match b {
                    b'-' => Scan::Comment((d + 1).min(2)),
                    b'>' if d >= 2 => {
                        self.safe_end = i + 1;
                        Scan::Text
                    }
                    _ => Scan::Comment(0),
                },
                Scan::Pi(prev_q) => {
                    if b == b'>' && prev_q {
                        self.safe_end = i + 1;
                        Scan::Text
                    } else {
                        Scan::Pi(b == b'?')
                    }
                }
            };
            if self.scan != Scan::Text && i + 1 - self.safe_end > max {
                return Err(corrupt("malformed XML: markup exceeds size limit"));
            }
            i += 1;
        }
        self.scan_pos = i;
        Ok(())
    }

    /// Lets a long text run through in pieces, cutting at a character boundary outside entities.
    fn maybe_flush_text(&mut self) {
        if self.scan != Scan::Text || self.buf.len() - self.safe_end < self.limits.text_flush {
            return;
        }
        let mut cut = self.buf.len();
        if let Some(a) = self.amp {
            // A reference longer than any legal one is left for the tokenizer to reject.
            if cut - a <= 64 {
                cut = a;
            }
        }
        if let Err(e) = std::str::from_utf8(&self.buf[self.safe_end..cut])
            && e.error_len().is_none()
        {
            cut = self.safe_end + e.valid_up_to();
        }
        if cut > self.safe_end {
            self.safe_end = cut;
        }
    }

    fn process(&mut self, end: usize, out: &mut Vec<XmlEvent>) -> Result<()> {
        let buf = std::mem::take(&mut self.buf);
        let res = self.process_slice(&buf[..end], out);
        self.buf = buf;
        self.buf.drain(..end);
        self.scan_pos = self.scan_pos.saturating_sub(end);
        self.safe_end = self.safe_end.saturating_sub(end);
        self.amp = self.amp.and_then(|a| a.checked_sub(end));
        res
    }

    fn process_slice(&mut self, bytes: &[u8], out: &mut Vec<XmlEvent>) -> Result<()> {
        let text =
            std::str::from_utf8(bytes).map_err(|_| corrupt("malformed XML: invalid UTF-8"))?;
        let text = if self.first_chunk {
            self.first_chunk = false;
            text.strip_prefix('\u{feff}').unwrap_or(text)
        } else {
            text
        };
        let mut reader = Reader::from_str(text);
        {
            let cfg = reader.config_mut();
            cfg.check_end_names = false;
            cfg.allow_unmatched_ends = true;
        }
        let mut pending = String::new();
        loop {
            match reader.read_event().map_err(xml_err)? {
                Event::Eof => break,
                Event::Start(e) => {
                    self.flush_text(&mut pending, out)?;
                    self.start_element(&e, out)?;
                }
                Event::Empty(e) => {
                    self.flush_text(&mut pending, out)?;
                    self.start_element(&e, out)?;
                    self.end_element(e.name().as_ref(), out)?;
                }
                Event::End(e) => {
                    self.flush_text(&mut pending, out)?;
                    self.end_element(e.name().as_ref(), out)?;
                }
                Event::Text(t) => pending.push_str(&t.xml10_content()),
                Event::CData(c) => pending.push_str(&c.xml10_content()),
                Event::GeneralRef(r) => {
                    if r.is_char_ref() {
                        let c = r
                            .resolve_char_ref()
                            .map_err(xml_err)?
                            .ok_or_else(|| corrupt("malformed XML: bad character reference"))?;
                        pending.push(c);
                    } else {
                        pending.push(match &*r.xml10_content() {
                            "amp" => '&',
                            "lt" => '<',
                            "gt" => '>',
                            "quot" => '"',
                            "apos" => '\'',
                            other => return Err(corrupt(format!("undefined entity &{other};"))),
                        });
                    }
                }
                Event::Comment(_) | Event::Decl(_) | Event::PI(_) => {}
                Event::DocType(_) => return Err(corrupt("DTD is not allowed")),
            }
        }
        self.flush_text(&mut pending, out)
    }

    fn flush_text(&self, pending: &mut String, out: &mut Vec<XmlEvent>) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        let text = std::mem::take(pending);
        if self.stack.is_empty() {
            if text.trim().is_empty() {
                return Ok(());
            }
            return Err(corrupt(
                "malformed XML: character data outside the root element",
            ));
        }
        out.push(XmlEvent::Text(text));
        Ok(())
    }

    fn lookup(&self, pending: &[(String, String)], prefix: &str) -> Option<String> {
        pending
            .iter()
            .rev()
            .chain(self.stack.iter().rev().flat_map(|s| s.decls.iter().rev()))
            .find(|(p, _)| p == prefix)
            .map(|(_, u)| u.clone())
    }

    fn resolve(&self, decls: &[(String, String)], lexical: &str, is_attr: bool) -> Result<Name> {
        match lexical.split_once(':') {
            Some(("xml", local)) => Ok(Name {
                ns: Ns::Xml,
                local: local.to_owned(),
            }),
            Some((prefix, local)) => {
                let uri = self
                    .lookup(decls, prefix)
                    .ok_or_else(|| corrupt(format!("unbound namespace prefix {prefix:?}")))?;
                Ok(Name {
                    ns: Ns::from_uri(&uri),
                    local: local.to_owned(),
                })
            }
            None => {
                let ns = if is_attr {
                    Ns::None
                } else {
                    self.lookup(decls, "")
                        .map_or(Ns::None, |u| Ns::from_uri(&u))
                };
                Ok(Name {
                    ns,
                    local: lexical.to_owned(),
                })
            }
        }
    }

    fn start_element(
        &mut self,
        e: &quick_xml::events::BytesStart<'_>,
        out: &mut Vec<XmlEvent>,
    ) -> Result<()> {
        if self.root_closed {
            return Err(corrupt("malformed XML: content after the root element"));
        }
        if self.stack.len() >= self.limits.max_depth {
            return Err(corrupt("malformed XML: nesting too deep"));
        }
        let lexical = e.name().as_ref().to_owned();
        let mut decls: Vec<(String, String)> = Vec::new();
        let mut raw_attrs: Vec<(String, String)> = Vec::new();
        for attr in e.attributes() {
            let attr = attr.map_err(xml_err)?;
            if decls.len() + raw_attrs.len() >= self.limits.max_attrs {
                return Err(corrupt("malformed XML: too many attributes"));
            }
            let key = attr.key.as_ref().to_owned();
            let value = attr
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(xml_err)?
                .into_owned();
            if key == "xmlns" {
                decls.push((String::new(), value));
            } else if let Some(prefix) = key.strip_prefix("xmlns:") {
                decls.push((prefix.to_owned(), value));
            } else {
                raw_attrs.push((key, value));
            }
        }
        let name = self.resolve(&decls, &lexical, false)?;
        let mut attrs = Vec::with_capacity(raw_attrs.len());
        for (key, value) in raw_attrs {
            attrs.push(Attr {
                name: self.resolve(&decls, &key, true)?,
                value,
            });
        }
        self.saw_root = true;
        self.stack.push(Scope { lexical, decls });
        out.push(XmlEvent::Start(StartTag { name, attrs }));
        Ok(())
    }

    fn end_element(&mut self, lexical: &str, out: &mut Vec<XmlEvent>) -> Result<()> {
        let open = self
            .stack
            .last()
            .map(|s| s.lexical.clone())
            .ok_or_else(|| corrupt("malformed XML: unmatched closing tag"))?;
        if open != lexical {
            return Err(corrupt(format!(
                "malformed XML: expected </{open}> but found </{lexical}>"
            )));
        }
        // Resolve while the element's own namespace declarations are still in scope.
        let name = self.resolve(&[], &open, false)?;
        self.stack.pop();
        if self.stack.is_empty() {
            self.root_closed = true;
        }
        out.push(XmlEvent::End(name));
        Ok(())
    }
}

/// Parses a whole in-memory document into events (test and small-body helper).
pub fn read_all(doc: &[u8]) -> Result<Vec<XmlEvent>> {
    let mut r = XmlReader::new();
    let mut events = r.feed(doc)?;
    events.extend(r.finish()?);
    Ok(events)
}

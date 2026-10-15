//! A small pull reader over one package part, and the size-capped text output.
//!
//! Element and attribute names are reported by local name (prefix dropped): the parts read here
//! use fixed vocabularies, so namespace URIs add nothing but cost.

use std::io::BufRead;

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};

use crate::ExtractError;

/// One simplified XML event. An empty element is reported as `Start` followed by `End`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ev {
    /// Element start with its attributes (local names, normalized values).
    Start(String, Vec<(String, String)>),
    /// Element end (local name).
    End(String),
    /// Character data, entity and character references resolved.
    Text(String),
    /// End of the part.
    Eof,
}

/// Pull reader enforcing a maximum nesting depth and refusing DTDs.
pub(crate) struct Pull<'r> {
    reader: Reader<&'r mut dyn BufRead>,
    buf: Vec<u8>,
    depth: usize,
    max_depth: usize,
    /// End event owed for an empty element.
    pending_end: Option<String>,
}

fn bad(e: impl std::fmt::Display) -> ExtractError {
    ExtractError::BadXml(e.to_string())
}

fn local(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_owned()
}

impl<'r> Pull<'r> {
    /// Reader over `src`.
    pub(crate) fn new(src: &'r mut dyn BufRead, max_depth: usize) -> Self {
        let mut reader = Reader::from_reader(src);
        reader.config_mut().check_end_names = true;
        Self {
            reader,
            buf: Vec::new(),
            depth: 0,
            max_depth,
            pending_end: None,
        }
    }

    /// Next event.
    pub(crate) fn next(&mut self) -> Result<Ev, ExtractError> {
        if let Some(name) = self.pending_end.take() {
            return Ok(Ev::End(name));
        }
        loop {
            self.buf.clear();
            let ev = self.reader.read_event_into(&mut self.buf).map_err(bad)?;
            return Ok(match ev {
                Event::Start(e) => {
                    if self.depth >= self.max_depth {
                        return Err(ExtractError::LimitExceeded("XML nesting is too deep"));
                    }
                    self.depth += 1;
                    Ev::Start(local(e.name().as_ref()), attrs(&e)?)
                }
                Event::Empty(e) => {
                    let name = local(e.name().as_ref());
                    let attrs = attrs(&e)?;
                    self.pending_end = Some(name.clone());
                    Ev::Start(name, attrs)
                }
                Event::End(e) => {
                    self.depth = self.depth.saturating_sub(1);
                    Ev::End(local(e.name().as_ref()))
                }
                Event::Text(t) => Ev::Text(t.xml10_content().into_owned()),
                Event::CData(c) => Ev::Text(c.xml10_content().into_owned()),
                Event::GeneralRef(r) => {
                    if r.is_char_ref() {
                        let c = r
                            .resolve_char_ref()
                            .map_err(bad)?
                            .ok_or_else(|| bad("bad character reference"))?;
                        Ev::Text(c.to_string())
                    } else {
                        let c = match &*r.xml10_content() {
                            "amp" => "&",
                            "lt" => "<",
                            "gt" => ">",
                            "quot" => "\"",
                            "apos" => "'",
                            other => return Err(bad(format!("undefined entity &{other};"))),
                        };
                        Ev::Text(c.to_owned())
                    }
                }
                Event::DocType(_) => return Err(bad("DTD is not allowed")),
                Event::Eof => Ev::Eof,
                Event::Decl(_) | Event::Comment(_) | Event::PI(_) => continue,
            });
        }
    }

    /// Skips to the end of the element whose `Start` was just returned.
    pub(crate) fn skip(&mut self) -> Result<(), ExtractError> {
        let mut depth = 1usize;
        while depth > 0 {
            match self.next()? {
                Ev::Start(..) => depth += 1,
                Ev::End(_) => depth -= 1,
                Ev::Text(_) => {}
                Ev::Eof => return Err(bad("unexpected end of the part")),
            }
        }
        Ok(())
    }
}

fn attrs(e: &quick_xml::events::BytesStart<'_>) -> Result<Vec<(String, String)>, ExtractError> {
    let mut out = Vec::new();
    for attr in e.attributes() {
        let attr = attr.map_err(bad)?;
        let value = attr
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(bad)?
            .into_owned();
        out.push((local(attr.key.as_ref()), value));
    }
    Ok(out)
}

/// Value of the attribute with local name `name`.
pub(crate) fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Produced text, refusing to grow beyond a byte limit.
pub(crate) struct Output {
    text: String,
    limit: usize,
}

impl Output {
    /// Empty output holding at most `limit` bytes.
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            limit,
        }
    }

    /// Appends `line` and a newline.
    pub(crate) fn line(&mut self, line: &str) -> Result<(), ExtractError> {
        if self.text.len().saturating_add(line.len()).saturating_add(1) > self.limit {
            return Err(ExtractError::LimitExceeded(
                "the extracted text is too long",
            ));
        }
        self.text.push_str(line);
        self.text.push('\n');
        Ok(())
    }

    /// Bytes produced so far.
    pub(crate) fn len(&self) -> usize {
        self.text.len()
    }

    /// Bytes still allowed.
    pub(crate) fn room(&self) -> usize {
        self.limit.saturating_sub(self.text.len())
    }

    /// Drops everything after the first `len` bytes (a length returned by [`Self::len`]).
    pub(crate) fn truncate(&mut self, len: usize) {
        self.text.truncate(len);
    }

    /// The text.
    pub(crate) fn into_string(self) -> String {
        self.text
    }
}

/// Text in one line: line breaks and tabs inside a value become visible escapes, so a value
/// never spreads over several diff lines.
pub(crate) fn one_line(s: &str) -> String {
    s.replace('\r', "").replace('\n', "\\n")
}

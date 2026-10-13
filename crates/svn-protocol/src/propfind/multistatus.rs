//! Streaming parser for `D:multistatus` documents.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use svn_types::Result;

use crate::http::ResponseParser;
use crate::util::corrupt;
use crate::xml::{Name, Ns, StartTag, XmlEvent, XmlReader};

/// Longest accepted property value (text), a guard against unbounded memory use.
const MAX_PROP_BYTES: usize = 8 << 20;

/// Value of one property, kept generic so one parser serves every PROPFIND.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PropValue {
    /// Character data directly inside the property element (raw, untrimmed).
    pub text: String,
    /// Local names of child elements (`resourcetype` holds `collection` for directories).
    pub child_names: Vec<String>,
    /// Text of every descendant element as `(local name, text)` in document order
    /// (`lockdiscovery` yields `owner`, `timeout`, `href`, ...; `checked-in` yields `href`).
    pub descendants: Vec<(String, String)>,
    /// Decoded bytes when the value was transported as base64 (`encoding="base64"`).
    pub decoded: Option<Vec<u8>>,
}

impl PropValue {
    /// Trimmed text.
    pub fn text(&self) -> &str {
        self.text.trim()
    }

    /// Value as bytes: the base64-decoded payload if present, otherwise the raw text.
    pub fn bytes(&self) -> &[u8] {
        self.decoded.as_deref().unwrap_or(self.text.as_bytes())
    }

    /// Text of the first descendant called `local`, trimmed.
    pub fn descendant(&self, local: &str) -> Option<&str> {
        self.descendants
            .iter()
            .find(|(n, _)| n == local)
            .map(|(_, t)| t.trim())
    }
}

/// One `D:response`: the href and the properties the server reported with a 2xx status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PropfindEntry {
    /// Raw href as sent (not yet validated; see `UrlBuilder::accept_href`).
    pub href: String,
    /// Properties with a successful status.
    pub found: Vec<(Name, PropValue)>,
    /// Properties the server reported as missing (4xx).
    pub missing: Vec<Name>,
}

impl PropfindEntry {
    /// Looks a found property up by namespace and local name.
    pub fn get(&self, ns: &Ns, local: &str) -> Option<&PropValue> {
        self.found
            .iter()
            .find(|(n, _)| n.is(ns, local))
            .map(|(_, v)| v)
    }

    /// Versioned (`svn:*`) and custom properties as `(name, value)`; DAV-level ones are skipped.
    pub fn versioned_props(&self) -> Vec<(String, Vec<u8>)> {
        self.found
            .iter()
            .filter_map(|(n, v)| match n.ns {
                Ns::SvnProps => Some((format!("svn:{}", n.local), v.bytes().to_vec())),
                Ns::Custom => Some((n.local.clone(), v.bytes().to_vec())),
                _ => None,
            })
            .collect()
    }
}

struct PropBuild {
    name: Name,
    value: PropValue,
    base64: bool,
    /// Open descendants: (local name, accumulated text).
    open: Vec<(String, String)>,
    size: usize,
}

/// Position inside the multistatus document, tracked as a stack of element names.
#[derive(Default)]
struct Cursor {
    path: Vec<Name>,
}

impl Cursor {
    fn is(&self, expect: &[(&Ns, &str)]) -> bool {
        self.path.len() == expect.len()
            && self.path.iter().zip(expect).all(|(n, (ns, l))| n.is(ns, l))
    }
}

/// Incremental `D:multistatus` parser producing one [`PropfindEntry`] per `D:response`.
#[derive(Default)]
pub struct MultiStatusParser {
    xml: XmlReader,
    cursor: Cursor,
    root_checked: bool,
    entry: PropfindEntry,
    in_href: bool,
    status_text: String,
    in_status: bool,
    stat_props: Vec<(Name, PropValue)>,
    stat_missing: Vec<Name>,
    prop: Option<PropBuild>,
}

impl MultiStatusParser {
    /// Creates a parser.
    pub fn new() -> Self {
        Self::default()
    }

    fn handle(&mut self, ev: XmlEvent, out: &mut Vec<PropfindEntry>) -> Result<()> {
        match ev {
            XmlEvent::Start(tag) => self.start(tag),
            XmlEvent::Text(t) => self.text(&t),
            XmlEvent::End(name) => self.end(name, out),
        }
    }

    fn start(&mut self, tag: StartTag) -> Result<()> {
        if !self.root_checked {
            if !tag.name.is(&Ns::Dav, "multistatus") {
                return Err(corrupt("expected a D:multistatus document"));
            }
            self.root_checked = true;
        }
        if let Some(p) = self.prop.as_mut() {
            // Inside a property value: record structure only.
            if p.open.is_empty() {
                p.value.child_names.push(tag.name.local.clone());
            }
            p.open.push((tag.name.local.clone(), String::new()));
            self.cursor.path.push(tag.name);
            return Ok(());
        }
        self.cursor.path.push(tag.name.clone());
        let (d, r, ps) = (&Ns::Dav, "response", "propstat");
        if self.cursor.is(&[(d, "multistatus"), (d, r)]) {
            self.entry = PropfindEntry::default();
        } else if self.cursor.is(&[(d, "multistatus"), (d, r), (d, "href")]) {
            self.in_href = true;
        } else if self.cursor.is(&[(d, "multistatus"), (d, r), (d, ps)]) {
            self.stat_props.clear();
            self.stat_missing.clear();
            self.status_text.clear();
        } else if self
            .cursor
            .is(&[(d, "multistatus"), (d, r), (d, ps), (d, "status")])
        {
            self.in_status = true;
        } else if self.cursor.path.len() == 5 && self.cursor.is_prop_child(d, r, ps) {
            let name = self.cursor.path[4].clone();
            let base64 = tag.attr("encoding").is_some_and(|e| e == "base64");
            self.prop = Some(PropBuild {
                name,
                value: PropValue::default(),
                base64,
                open: Vec::new(),
                size: 0,
            });
        }
        Ok(())
    }

    fn text(&mut self, t: &str) -> Result<()> {
        if let Some(p) = self.prop.as_mut() {
            p.size += t.len();
            if p.size > MAX_PROP_BYTES {
                return Err(corrupt("property value too large"));
            }
            match p.open.last_mut() {
                Some((_, buf)) => buf.push_str(t),
                None => p.value.text.push_str(t),
            }
        } else if self.in_href {
            self.entry.href.push_str(t);
        } else if self.in_status {
            self.status_text.push_str(t);
        }
        Ok(())
    }

    fn end(&mut self, name: Name, out: &mut Vec<PropfindEntry>) -> Result<()> {
        let was_len = self.cursor.path.len();
        if let Some(p) = self.prop.as_mut() {
            if let Some((local, text)) = p.open.pop() {
                p.value.descendants.push((local, text));
                self.cursor.path.pop();
                return Ok(());
            }
            // The property element itself closes.
            let mut done = self
                .prop
                .take()
                .ok_or_else(|| corrupt("internal parser state"))?;
            if done.base64 {
                let compact: String = done
                    .value
                    .text
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                let bytes = STANDARD.decode(compact.as_bytes()).map_err(|e| {
                    corrupt(format!("bad base64 in property {}: {e}", done.name.local))
                })?;
                done.value.decoded = Some(bytes);
            }
            self.stat_props.push((done.name, done.value));
            self.cursor.path.pop();
            return Ok(());
        }
        let (d, r, ps) = (&Ns::Dav, "response", "propstat");
        if was_len == 3 && self.cursor.is(&[(d, "multistatus"), (d, r), (d, "href")]) {
            self.in_href = false;
            self.entry.href = self.entry.href.trim().to_owned();
        } else if self
            .cursor
            .is(&[(d, "multistatus"), (d, r), (d, ps), (d, "status")])
        {
            self.in_status = false;
        } else if self.cursor.is(&[(d, "multistatus"), (d, r), (d, ps)]) {
            let ok = status_is_success(&self.status_text);
            if ok {
                self.entry.found.append(&mut self.stat_props);
            } else {
                self.entry
                    .missing
                    .extend(self.stat_props.drain(..).map(|(n, _)| n));
            }
        } else if self.cursor.is(&[(d, "multistatus"), (d, r)]) {
            out.push(std::mem::take(&mut self.entry));
        }
        debug_assert!(self.cursor.path.last().is_some_and(|n| *n == name));
        self.cursor.path.pop();
        Ok(())
    }

    fn run(&mut self, events: Vec<XmlEvent>) -> Result<Vec<PropfindEntry>> {
        let mut out = Vec::new();
        for ev in events {
            self.handle(ev, &mut out)?;
        }
        Ok(out)
    }
}

impl Cursor {
    /// Whether the path is `multistatus/response/propstat/prop/<anything>`.
    fn is_prop_child(&self, d: &Ns, r: &str, ps: &str) -> bool {
        self.path.len() == 5
            && self.path[..4]
                .iter()
                .zip([(d, "multistatus"), (d, r), (d, ps), (d, "prop")])
                .all(|(n, (ns, l))| n.is(ns, l))
    }
}

/// `HTTP/1.1 200 OK` style status line: success when the code is 2xx.
fn status_is_success(line: &str) -> bool {
    line.split_whitespace()
        .nth(1)
        .is_some_and(|c| c.starts_with('2'))
}

impl ResponseParser for MultiStatusParser {
    type Item = PropfindEntry;

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<PropfindEntry>> {
        let events = self.xml.feed(chunk)?;
        self.run(events)
    }

    fn finish(&mut self) -> Result<Vec<PropfindEntry>> {
        let events = self.xml.finish()?;
        let out = self.run(events)?;
        if !self.root_checked {
            return Err(corrupt("empty multistatus document"));
        }
        Ok(out)
    }
}

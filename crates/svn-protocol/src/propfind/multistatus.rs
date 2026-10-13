//! Streaming parser for `D:multistatus` documents.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use svn_types::Result;

use crate::http::ResponseParser;
use crate::limits::ParserLimits;
use crate::util::corrupt;
use crate::xml::{Name, Ns, StartTag, XmlEvent, XmlReader};

/// Value of one property, kept generic so one parser serves every PROPFIND.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PropValue {
    /// Character data directly inside the property element (raw, untrimmed).
    pub text: String,
    /// Local names of child elements (`resourcetype` holds `collection` for directories).
    pub child_names: Vec<String>,
    /// Text of every descendant element as `(path, text)` in closing order, where `path` is the
    /// slash-joined local names below the property (`activelock/locktoken/href`, `href`).
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

    /// Text of the descendant at exactly `path` (for example `"activelock/locktoken/href"`),
    /// trimmed.
    pub fn descendant_at(&self, path: &str) -> Option<&str> {
        self.descendants
            .iter()
            .find(|(p, _)| p == path)
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
    elements: usize,
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
    limits: ParserLimits,
    entry_props: usize,
    entry_bytes: usize,
    failed: bool,
}

impl MultiStatusParser {
    /// Creates a parser with the default limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a parser with explicit limits.
    pub fn with_limits(limits: ParserLimits) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }

    fn handle(&mut self, ev: XmlEvent, out: &mut Vec<PropfindEntry>) -> Result<()> {
        match ev {
            XmlEvent::Start(tag) => self.start(tag),
            XmlEvent::Text(t) => self.text(&t),
            XmlEvent::End(_) => self.end(out),
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
            p.elements += 1;
            ParserLimits::check(
                p.elements,
                self.limits.max_prop_elements,
                "property elements",
            )?;
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
            self.entry_props = 0;
            self.entry_bytes = 0;
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
            let encoding = tag
                .attr_in(&Ns::SvnDav, "encoding")
                .or_else(|| tag.attr("encoding"));
            let base64 = match encoding {
                None => false,
                Some("base64") => true,
                Some(other) => return Err(corrupt(format!("unknown value encoding {other:?}"))),
            };
            self.entry_props += 1;
            ParserLimits::check(
                self.entry_props,
                self.limits.max_props_per_entry,
                "properties per response",
            )?;
            self.prop = Some(PropBuild {
                elements: 0,
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
            ParserLimits::check(p.size, self.limits.max_text_bytes, "property value")?;
            self.entry_bytes += t.len();
            ParserLimits::check(
                self.entry_bytes,
                self.limits.max_entry_bytes,
                "properties of one response",
            )?;
            match p.open.last_mut() {
                Some((_, buf)) => buf.push_str(t),
                None => p.value.text.push_str(t),
            }
        } else if self.in_href {
            self.entry.href.push_str(t);
            ParserLimits::check(self.entry.href.len(), self.limits.max_href_bytes, "href")?;
        } else if self.in_status {
            self.status_text.push_str(t);
            ParserLimits::check(
                self.status_text.len(),
                self.limits.max_status_bytes,
                "status",
            )?;
        }
        Ok(())
    }

    fn end(&mut self, out: &mut Vec<PropfindEntry>) -> Result<()> {
        let was_len = self.cursor.path.len();
        if let Some(p) = self.prop.as_mut() {
            if !p.open.is_empty() {
                let path = p
                    .open
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join("/");
                if let Some((_, text)) = p.open.pop() {
                    p.value.descendants.push((path, text));
                }
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
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.feed(chunk).and_then(|events| self.run(events));
        self.failed = result.is_err();
        result
    }

    fn finish(&mut self) -> Result<Vec<PropfindEntry>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.finish().and_then(|events| {
            let out = self.run(events)?;
            if self.root_checked {
                Ok(out)
            } else {
                Err(corrupt("empty multistatus document"))
            }
        });
        self.failed = result.is_err();
        result
    }
}

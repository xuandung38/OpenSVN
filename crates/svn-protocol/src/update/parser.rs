//! Streaming `S:update-report` response parser (send-all and skelta).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use svn_types::{RelPath, Result, Revnum};

use super::events::{Md5, Sha1, UpdateEvent};
use crate::caps::{AcceptedHref, UrlBuilder};
use crate::http::{ResponseParser, parse_chunked};
use crate::limits::ParserLimits;
use crate::util::{Base64Stream, corrupt, parse_hex, parse_revnum};
use crate::xml::{Ns, StartTag, XmlEvent, XmlReader};

/// Deepest directory nesting accepted.
const MAX_NODE_DEPTH: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dir,
    File,
}

/// How the node was introduced; decides which event announces it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Intro {
    Open,
    Add,
}

struct Node {
    kind: Kind,
    intro: Intro,
    path: RelPath,
    base_rev: Option<Revnum>,
    sha1: Option<Sha1>,
    checked_in: Option<AcceptedHref>,
    announced: bool,
    md5: Option<Md5>,
}

enum Capture {
    None,
    Href,
    Md5,
    Prop {
        name: String,
        base64: bool,
        text: String,
    },
    TxDelta(Base64Stream),
}

/// Incremental update-report parser. See [`UpdateEvent`].
pub struct UpdateParser {
    urls: UrlBuilder,
    xml: XmlReader,
    depth: usize,
    root_seen: bool,
    nodes: Vec<Node>,
    capture: Capture,
    capture_depth: usize,
    value: String,
    limits: ParserLimits,
    failed: bool,
}

fn node_name(tag: &StartTag) -> Result<&str> {
    tag.attr("name")
        .filter(|n| !n.is_empty() && !n.contains('/'))
        .ok_or_else(|| corrupt("update entry without a valid name"))
}

fn is_svn(tag: &StartTag, local: &str) -> bool {
    tag.name.is(&Ns::Svn, local)
}

impl UpdateParser {
    /// Creates a parser; `urls` vets every `D:checked-in` href.
    pub fn new(urls: UrlBuilder) -> Self {
        Self {
            urls,
            xml: XmlReader::new(),
            depth: 0,
            root_seen: false,
            nodes: Vec::new(),
            capture: Capture::None,
            capture_depth: 0,
            value: String::new(),
            limits: ParserLimits::default(),
            failed: false,
        }
    }

    /// Creates a parser with explicit limits.
    pub fn with_limits(urls: UrlBuilder, limits: ParserLimits) -> Self {
        Self {
            limits,
            ..Self::new(urls)
        }
    }

    fn child_path(&self, name: &str) -> Result<RelPath> {
        match self.nodes.last() {
            Some(parent) if !parent.path.is_root() => {
                RelPath::from_server(&format!("{}/{name}", parent.path))
            }
            _ => RelPath::from_server(name),
        }
    }

    fn current_path(&self) -> RelPath {
        self.nodes
            .last()
            .map_or_else(RelPath::root, |n| n.path.clone())
    }

    /// Emits the announcement of the innermost node unless it already happened.
    fn announce(&mut self, out: &mut Vec<UpdateEvent>) {
        let Some(node) = self.nodes.last_mut() else {
            return;
        };
        if node.announced {
            return;
        }
        node.announced = true;
        let path = node.path.clone();
        let checked_in = node.checked_in.clone();
        out.push(match (node.kind, node.intro) {
            (Kind::Dir, Intro::Open) => UpdateEvent::OpenDir {
                path,
                base_rev: node.base_rev,
                checked_in,
            },
            (Kind::Dir, Intro::Add) => UpdateEvent::AddDir { path, checked_in },
            (Kind::File, Intro::Open) => UpdateEvent::OpenFile {
                path,
                base_rev: node.base_rev,
                checked_in,
            },
            (Kind::File, Intro::Add) => UpdateEvent::AddFile {
                path,
                checked_in,
                sha1: node.sha1,
            },
        });
    }

    fn push_node(
        &mut self,
        tag: &StartTag,
        kind: Kind,
        intro: Intro,
        out: &mut Vec<UpdateEvent>,
    ) -> Result<()> {
        if self.nodes.len() >= MAX_NODE_DEPTH {
            return Err(corrupt("update-report nests too deeply"));
        }
        let path = if self.nodes.is_empty() && tag.attr("name").is_none() {
            RelPath::root()
        } else {
            self.child_path(node_name(tag)?)?
        };
        self.announce(out);
        self.nodes.push(Node {
            kind,
            intro,
            path,
            base_rev: tag.attr("rev").map(parse_revnum).transpose()?,
            sha1: tag.attr("sha1-checksum").map(parse_hex::<20>).transpose()?,
            checked_in: None,
            announced: false,
            md5: None,
        });
        Ok(())
    }

    fn start(&mut self, tag: &StartTag, out: &mut Vec<UpdateEvent>) -> Result<()> {
        self.depth += 1;
        if self.depth == 1 {
            if !is_svn(tag, "update-report") {
                return Err(corrupt("expected an S:update-report document"));
            }
            self.root_seen = true;
            return Ok(());
        }
        if !matches!(self.capture, Capture::None) {
            return Ok(()); // markup inside a captured value is ignored
        }
        if tag.name.is(&Ns::Dav, "checked-in") {
            return Ok(()); // its D:href child carries the value
        }
        if tag.name.is(&Ns::Dav, "href") {
            if self.nodes.last().is_some_and(|n| !n.announced) {
                self.capture = Capture::Href;
            }
            return Ok(());
        }
        if tag.name.local == "md5-checksum" && self.nodes.last().is_some() {
            self.capture = Capture::Md5;
            return Ok(());
        }
        if tag.name.ns != Ns::Svn {
            return Ok(());
        }
        match tag.name.local.as_str() {
            "target-revision" => {
                let rev = tag
                    .attr("rev")
                    .ok_or_else(|| corrupt("target-revision without rev"))?;
                out.push(UpdateEvent::TargetRevision(parse_revnum(rev)?));
            }
            "open-directory" => self.push_node(tag, Kind::Dir, Intro::Open, out)?,
            "add-directory" => self.push_node(tag, Kind::Dir, Intro::Add, out)?,
            "open-file" => self.push_node(tag, Kind::File, Intro::Open, out)?,
            "add-file" => self.push_node(tag, Kind::File, Intro::Add, out)?,
            "delete-entry" | "absent-file" | "absent-directory" => {
                self.announce(out);
                let path = self.child_path(node_name(tag)?)?;
                out.push(match tag.name.local.as_str() {
                    "delete-entry" => UpdateEvent::DeleteEntry { path },
                    "absent-file" => UpdateEvent::AbsentFile { path },
                    _ => UpdateEvent::AbsentDir { path },
                });
            }
            "set-prop" => {
                self.announce(out);
                let name = tag
                    .attr("name")
                    .filter(|n| !n.is_empty())
                    .ok_or_else(|| corrupt("set-prop without a name"))?
                    .to_owned();
                self.capture = Capture::Prop {
                    name,
                    base64: tag.attr("encoding") == Some("base64"),
                    text: String::new(),
                };
            }
            "remove-prop" => {
                self.announce(out);
                let name = tag
                    .attr("name")
                    .filter(|n| !n.is_empty())
                    .ok_or_else(|| corrupt("remove-prop without a name"))?
                    .to_owned();
                out.push(UpdateEvent::RemoveProp {
                    path: self.current_path(),
                    name,
                });
            }
            "fetch-props" => {
                self.announce(out);
                let href = self.nodes.last().and_then(|n| n.checked_in.clone());
                out.push(UpdateEvent::FetchProps {
                    path: self.current_path(),
                    href,
                });
            }
            "fetch-file" => {
                self.announce(out);
                let href = self.nodes.last().and_then(|n| n.checked_in.clone());
                out.push(UpdateEvent::FetchFile {
                    path: self.current_path(),
                    href,
                    base_checksum: tag.attr("base-checksum").map(parse_hex::<16>).transpose()?,
                });
            }
            "txdelta" => {
                self.announce(out);
                out.push(UpdateEvent::TxDeltaStart {
                    path: self.current_path(),
                    base_checksum: tag.attr("base-checksum").map(parse_hex::<16>).transpose()?,
                });
                self.capture = Capture::TxDelta(Base64Stream::default());
            }
            _ => {} // `S:prop` and anything newer: containers and unknown elements are skipped
        }
        Ok(())
    }

    fn text(&mut self, text: &str, out: &mut Vec<UpdateEvent>) -> Result<()> {
        match &mut self.capture {
            Capture::None => {}
            Capture::Href | Capture::Md5 => {
                self.value.push_str(text);
                ParserLimits::check(
                    self.value.len(),
                    self.limits.max_href_bytes,
                    "href or checksum",
                )?;
            }
            Capture::Prop { text: buf, .. } => {
                buf.push_str(text);
                ParserLimits::check(buf.len(), self.limits.max_text_bytes, "property value")?;
            }
            Capture::TxDelta(b64) => {
                let data = b64.push(text)?;
                if !data.is_empty() {
                    out.push(UpdateEvent::TxDeltaChunk {
                        path: self.current_path(),
                        data: Bytes::from(data),
                    });
                }
            }
        }
        Ok(())
    }

    fn end(&mut self, local: &str, out: &mut Vec<UpdateEvent>) -> Result<()> {
        let was_depth = self.depth;
        self.depth = self.depth.saturating_sub(1);
        if !matches!(self.capture, Capture::None) {
            if was_depth != self.capture_depth {
                return Ok(()); // end of markup nested inside a captured value
            }
            return self.finish_capture(out);
        }
        if matches!(
            local,
            "open-directory" | "add-directory" | "open-file" | "add-file"
        ) {
            self.announce(out);
            if let Some(node) = self.nodes.pop() {
                out.push(match node.kind {
                    Kind::Dir => UpdateEvent::CloseDir { path: node.path },
                    Kind::File => UpdateEvent::CloseFile {
                        path: node.path,
                        md5: node.md5,
                    },
                });
            }
        }
        Ok(())
    }

    fn finish_capture(&mut self, out: &mut Vec<UpdateEvent>) -> Result<()> {
        match std::mem::replace(&mut self.capture, Capture::None) {
            Capture::None => {}
            Capture::Href => {
                let href = std::mem::take(&mut self.value);
                let accepted = self.urls.accept_href(href.trim())?;
                if let Some(n) = self.nodes.last_mut() {
                    n.checked_in = Some(accepted);
                }
            }
            Capture::Md5 => {
                let md5 = parse_hex::<16>(&std::mem::take(&mut self.value))?;
                if let Some(n) = self.nodes.last_mut() {
                    n.md5 = Some(md5);
                }
            }
            Capture::Prop { name, base64, text } => {
                let value = if base64 {
                    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
                    STANDARD
                        .decode(compact.as_bytes())
                        .map_err(|e| corrupt(format!("bad base64 in property {name}: {e}")))?
                } else {
                    text.into_bytes()
                };
                out.push(UpdateEvent::SetProp {
                    path: self.current_path(),
                    name,
                    value: Bytes::from(value),
                });
            }
            Capture::TxDelta(mut b64) => {
                b64.finish()?;
                out.push(UpdateEvent::TxDeltaEnd {
                    path: self.current_path(),
                });
            }
        }
        Ok(())
    }

    fn run(&mut self, events: Vec<XmlEvent>) -> Result<Vec<UpdateEvent>> {
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
                XmlEvent::End(name) => {
                    self.end(&name.local, &mut out)?;
                    if matches!(self.capture, Capture::None) {
                        self.capture_depth = 0;
                    }
                }
            }
        }
        Ok(out)
    }
}

impl ResponseParser for UpdateParser {
    type Item = UpdateEvent;

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<UpdateEvent>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.feed(chunk).and_then(|events| self.run(events));
        self.failed = result.is_err();
        result
    }

    fn finish(&mut self) -> Result<Vec<UpdateEvent>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.finish().and_then(|events| {
            let out = self.run(events)?;
            if self.root_seen && self.nodes.is_empty() {
                Ok(out)
            } else {
                Err(corrupt(
                    "update-report ended before all entries were closed",
                ))
            }
        });
        self.failed = result.is_err();
        result
    }
}

/// Parses a complete update-report body (tests and small reports).
pub fn parse_update(urls: &UrlBuilder, body: &[u8]) -> Result<Vec<UpdateEvent>> {
    parse_chunked(&mut UpdateParser::new(urls.clone()), body, 0)
}

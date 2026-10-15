//! Streaming `S:update-report` response parser (send-all and skelta).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use svn_types::{RelPath, Result, Revnum, SvnError};

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
    copy_from: Option<(String, Revnum)>,
    checked_in: Option<AcceptedHref>,
    announced: bool,
    md5: Option<Md5>,
    /// A `txdelta` or `fetch-file` was seen (at most one per file).
    content_seen: bool,
    props_fetch_seen: bool,
    href_seen: bool,
}

/// Role of an open XML element; the stack of roles makes end tags match by namespace and name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Report,
    Node,
    CheckedIn,
    Other,
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
///
/// The parser is the gate for the drive order: it accepts exactly one root `open-directory`,
/// children only below directories, at most one `txdelta`/`fetch-file` per file and nothing after
/// the root closes. Any violation is `ProtocolViolation`, so a consumer can rely on the event
/// stream being a valid editor drive.
pub struct UpdateParser {
    urls: UrlBuilder,
    xml: XmlReader,
    roles: Vec<(crate::xml::Name, Role)>,
    nodes: Vec<Node>,
    started: bool,
    send_all: bool,
    inline_props: bool,
    target_seen: bool,
    root_seen: bool,
    root_closed: bool,
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

fn flag(tag: &StartTag, name: &str) -> bool {
    tag.attr(name) == Some("true")
}

impl UpdateParser {
    /// Creates a parser; `urls` vets every `D:checked-in` href.
    pub fn new(urls: UrlBuilder) -> Self {
        Self {
            urls,
            xml: XmlReader::new(),
            roles: Vec::new(),
            nodes: Vec::new(),
            started: false,
            send_all: false,
            inline_props: false,
            target_seen: false,
            root_seen: false,
            root_closed: false,
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
        let full = match self.nodes.last() {
            Some(parent) if !parent.path.is_root() => format!("{}/{name}", parent.path),
            _ => name.to_owned(),
        };
        RelPath::from_server(&full).map_err(|e| {
            // Names the working copy cannot represent safely (`..`, `.svn`, `a:b`, `CON`, ...)
            // stop the whole update with the offending path in the message.
            SvnError::new(
                e.code,
                format!(
                    "the update would create the unsafe path {full:?}: {}",
                    e.message
                ),
            )
        })
    }

    fn current_path(&self) -> RelPath {
        self.nodes
            .last()
            .map_or_else(RelPath::root, |n| n.path.clone())
    }

    /// Innermost open element is a node of the given kind.
    fn in_node(&self, kind: Kind) -> bool {
        self.roles.last().is_some_and(|(_, r)| *r == Role::Node)
            && self.nodes.last().is_some_and(|n| n.kind == kind)
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
        let copy_from = node.copy_from.clone();
        out.push(match (node.kind, node.intro) {
            (Kind::Dir, Intro::Open) => UpdateEvent::OpenDir {
                path,
                base_rev: node.base_rev,
                checked_in,
            },
            (Kind::Dir, Intro::Add) => UpdateEvent::AddDir {
                path,
                checked_in,
                copy_from,
            },
            (Kind::File, Intro::Open) => UpdateEvent::OpenFile {
                path,
                base_rev: node.base_rev,
                checked_in,
            },
            (Kind::File, Intro::Add) => UpdateEvent::AddFile {
                path,
                checked_in,
                sha1: node.sha1,
                copy_from,
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
        let is_root = self.nodes.is_empty();
        if is_root {
            if self.root_closed || self.root_seen {
                return Err(corrupt("update-report has more than one root directory"));
            }
            if kind != Kind::Dir || intro != Intro::Open || tag.attr("name").is_some() {
                return Err(corrupt(
                    "update-report root must be an anonymous open-directory",
                ));
            }
            if self.roles.last().map(|(_, r)| *r) != Some(Role::Report) {
                return Err(corrupt("update-report root is misplaced"));
            }
        } else if !self.in_node(Kind::Dir) {
            return Err(corrupt("update-report entry is not inside a directory"));
        }
        let path = if is_root {
            RelPath::root()
        } else {
            self.child_path(node_name(tag)?)?
        };
        let copy_from = match (tag.attr("copyfrom-path"), tag.attr("copyfrom-rev")) {
            (None, None) => None,
            (Some(p), Some(r)) if intro == Intro::Add => Some((p.to_owned(), parse_revnum(r)?)),
            _ => return Err(corrupt("inconsistent copyfrom attributes")),
        };
        self.announce(out);
        self.root_seen = true;
        self.nodes.push(Node {
            kind,
            intro,
            path,
            base_rev: tag.attr("rev").map(parse_revnum).transpose()?,
            sha1: tag.attr("sha1-checksum").map(parse_hex::<20>).transpose()?,
            copy_from,
            checked_in: None,
            announced: false,
            md5: None,
            content_seen: false,
            props_fetch_seen: false,
            href_seen: false,
        });
        Ok(())
    }

    fn start(&mut self, tag: &StartTag, out: &mut Vec<UpdateEvent>) -> Result<()> {
        if self.roles.is_empty() {
            if self.started || !tag.name.is(&Ns::Svn, "update-report") {
                return Err(corrupt("expected an S:update-report document"));
            }
            self.started = true;
            self.send_all = flag(tag, "send-all");
            self.inline_props = flag(tag, "inline-props");
            self.roles.push((tag.name.clone(), Role::Report));
            out.push(UpdateEvent::ReportStart {
                send_all: self.send_all,
                inline_props: self.inline_props,
            });
            return Ok(());
        }
        if !matches!(self.capture, Capture::None) {
            self.roles.push((tag.name.clone(), Role::Other));
            return Ok(()); // markup inside a captured value is ignored
        }
        let role = self.start_known(tag, out)?;
        self.roles.push((tag.name.clone(), role));
        Ok(())
    }

    fn start_known(&mut self, tag: &StartTag, out: &mut Vec<UpdateEvent>) -> Result<Role> {
        let parent = self.roles.last().map(|(_, r)| *r);
        if tag.name.is(&Ns::Dav, "checked-in") {
            return Ok(if parent == Some(Role::Node) {
                Role::CheckedIn
            } else {
                Role::Other
            });
        }
        if tag.name.is(&Ns::Dav, "href") {
            if parent == Some(Role::CheckedIn)
                && let Some(n) = self.nodes.last_mut()
            {
                if n.href_seen {
                    return Err(corrupt("checked-in carries more than one href"));
                }
                n.href_seen = true;
                if !n.announced {
                    self.capture = Capture::Href;
                }
            }
            return Ok(Role::Other);
        }
        if tag.name.local == "md5-checksum" && self.in_file_scope() {
            self.capture = Capture::Md5;
            return Ok(Role::Other);
        }
        if tag.name.ns != Ns::Svn {
            return Ok(Role::Other);
        }
        let known = matches!(
            tag.name.local.as_str(),
            "target-revision"
                | "open-directory"
                | "add-directory"
                | "open-file"
                | "add-file"
                | "delete-entry"
                | "absent-file"
                | "absent-directory"
                | "set-prop"
                | "remove-prop"
                | "fetch-props"
                | "fetch-file"
                | "txdelta"
        );
        if known && self.root_closed {
            return Err(corrupt(
                "update-report continues after the root directory closed",
            ));
        }
        match tag.name.local.as_str() {
            "target-revision" => {
                if parent != Some(Role::Report) || self.target_seen || self.root_seen {
                    return Err(corrupt("misplaced or repeated target-revision"));
                }
                self.target_seen = true;
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
                if !self.in_node(Kind::Dir) {
                    return Err(corrupt("delete or absent entry outside a directory"));
                }
                self.announce(out);
                let path = self.child_path(node_name(tag)?)?;
                out.push(match tag.name.local.as_str() {
                    "delete-entry" => UpdateEvent::DeleteEntry {
                        path,
                        rev: tag.attr("rev").map(parse_revnum).transpose()?,
                    },
                    "absent-file" => UpdateEvent::AbsentFile { path },
                    _ => UpdateEvent::AbsentDir { path },
                });
                return Ok(Role::Other);
            }
            "set-prop" | "remove-prop" => {
                if parent != Some(Role::Node) {
                    return Err(corrupt("property change outside a directory or file"));
                }
                self.announce(out);
                let name = tag
                    .attr("name")
                    .filter(|n| !n.is_empty())
                    .ok_or_else(|| corrupt("property change without a name"))?
                    .to_owned();
                if tag.name.local == "remove-prop" {
                    out.push(UpdateEvent::RemoveProp {
                        path: self.current_path(),
                        name,
                    });
                } else {
                    let base64 = match tag.attr("encoding") {
                        None => false,
                        Some("base64") => true,
                        Some(other) => {
                            return Err(corrupt(format!("unknown value encoding {other:?}")));
                        }
                    };
                    self.capture = Capture::Prop {
                        name,
                        base64,
                        text: String::new(),
                    };
                }
            }
            "fetch-props" => {
                if parent != Some(Role::Node) {
                    return Err(corrupt("fetch-props outside a directory or file"));
                }
                self.announce(out);
                let href = self.nodes.last().and_then(|n| n.checked_in.clone());
                if let Some(n) = self.nodes.last_mut() {
                    n.props_fetch_seen = true;
                }
                out.push(UpdateEvent::FetchProps {
                    path: self.current_path(),
                    href,
                });
            }
            "fetch-file" | "txdelta" => {
                if !self.in_node(Kind::File) {
                    return Err(corrupt("file content outside a file"));
                }
                if self.nodes.last().is_some_and(|n| n.content_seen) {
                    return Err(corrupt("a file has more than one text delta"));
                }
                self.announce(out);
                if let Some(n) = self.nodes.last_mut() {
                    n.content_seen = true;
                }
                let base_checksum = tag.attr("base-checksum").map(parse_hex::<16>).transpose()?;
                if tag.name.local == "txdelta" {
                    out.push(UpdateEvent::TxDeltaStart {
                        path: self.current_path(),
                        base_checksum,
                    });
                    self.capture = Capture::TxDelta(Base64Stream::default());
                } else {
                    let href = self.nodes.last().and_then(|n| n.checked_in.clone());
                    out.push(UpdateEvent::FetchFile {
                        path: self.current_path(),
                        href,
                        base_checksum,
                    });
                }
            }
            _ => return Ok(Role::Other), // `S:prop` and anything newer
        }
        Ok(match tag.name.local.as_str() {
            "open-directory" | "add-directory" | "open-file" | "add-file" => Role::Node,
            _ => Role::Other,
        })
    }

    /// Inside a file node, possibly below its `S:prop` container.
    fn in_file_scope(&self) -> bool {
        self.nodes.last().is_some_and(|n| n.kind == Kind::File)
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

    fn end(&mut self, out: &mut Vec<UpdateEvent>) -> Result<()> {
        let depth = self.roles.len();
        let (_, role) = self
            .roles
            .pop()
            .ok_or_else(|| corrupt("unmatched closing tag"))?;
        if !matches!(self.capture, Capture::None) {
            if depth != self.capture_depth {
                return Ok(()); // end of markup nested inside a captured value
            }
            return self.finish_capture(out);
        }
        if role == Role::Node {
            self.announce(out);
            let Some(node) = self.nodes.pop() else {
                return Err(corrupt("closing tag without an open entry"));
            };
            // Skelta answers carry no text: an added file must be fetched, or it would stay empty.
            // Opened files that only changed properties keep their text.
            if node.kind == Kind::File
                && node.intro == Intro::Add
                && !self.send_all
                && !node.content_seen
            {
                out.push(UpdateEvent::FetchFile {
                    path: node.path.clone(),
                    href: node.checked_in.clone(),
                    base_checksum: None,
                });
            }
            if !self.send_all
                && !self.inline_props
                && node.intro == Intro::Add
                && !node.props_fetch_seen
            {
                out.push(UpdateEvent::FetchProps {
                    path: node.path.clone(),
                    href: node.checked_in.clone(),
                });
            }
            out.push(match node.kind {
                Kind::Dir => UpdateEvent::CloseDir { path: node.path },
                Kind::File => UpdateEvent::CloseFile {
                    path: node.path,
                    md5: node.md5,
                },
            });
            if self.nodes.is_empty() {
                self.root_closed = true;
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
                        self.capture_depth = self.roles.len();
                    }
                }
                XmlEvent::Text(t) => self.text(&t, &mut out)?,
                XmlEvent::End(name) => {
                    if self.roles.last().is_none_or(|(n, _)| *n != name) {
                        return Err(corrupt("closing tag does not match the open element"));
                    }
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
            if self.started && self.nodes.is_empty() {
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

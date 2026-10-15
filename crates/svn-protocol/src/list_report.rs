//! `list-report`: one-request directory listing for servers that advertise the `list` capability.
//!
//! Servers without it are listed with a `Depth: 1` PROPFIND (see [`crate::propfind`]);
//! [`crate::Capabilities::list_strategy`] says which to use.

use svn_types::{DirEntry, NodeKind, RelPath, Result, Revnum};

use crate::caps::{UrlBuilder, advertise_caps};
use crate::http::{HttpRequest, Method, ResponseParser, parse_chunked};
use crate::limits::ParserLimits;
use crate::propfind::{ListItem, RejectedEntry};
use crate::util::{corrupt, decode_display_field, parse_revnum, parse_timestamp};
use crate::xml::{Ns, XmlEvent, XmlReader, XmlWriter};

/// How deep the listing goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListDepth {
    /// Only the node itself.
    Empty,
    /// The directory and its file children.
    Files,
    /// The directory and all immediate children.
    Immediates,
    /// The whole subtree.
    Infinity,
}

impl ListDepth {
    fn word(self) -> &'static str {
        match self {
            ListDepth::Empty => "empty",
            ListDepth::Files => "files",
            ListDepth::Immediates => "immediates",
            ListDepth::Infinity => "infinity",
        }
    }
}

/// Builds `REPORT rvr/REV/dir` with an `S:list-report` body asking for the properties that make
/// up a [`DirEntry`].
pub fn list_report_request(
    urls: &UrlBuilder,
    rev: Revnum,
    dir: &RelPath,
    depth: ListDepth,
) -> Result<HttpRequest> {
    let mut w = XmlWriter::new();
    w.start("S:list-report", &[("xmlns:S", "svn:")])
        .element("S:path", &[], "")
        .element("S:revision", &[], &rev.0.to_string())
        .element("S:depth", &[], depth.word());
    for prop in [
        "DAV:resourcetype",
        "DAV:getcontentlength",
        "DAV:version-name",
        "DAV:creationdate",
        "DAV:creator-displayname",
    ] {
        w.element("S:prop", &[], prop);
    }
    w.end();
    let req = HttpRequest::new(Method::Report, urls.rev_root(rev, dir)?).xml_body(w.finish()?);
    Ok(advertise_caps(req))
}

struct Item {
    kind: NodeKind,
    size: Option<u64>,
    rev: Option<Revnum>,
    date: Option<String>,
    path: String,
    author: Option<String>,
}

/// Incremental parser for `S:list-report`. `dir` is the directory that was listed; its own item
/// is reported as [`ListItem::Dir`], direct children as [`ListItem::Entry`].
pub struct ListReportParser {
    dir: RelPath,
    xml: XmlReader,
    depth: usize,
    item: Option<Item>,
    in_author: Option<Option<String>>,
    text: String,
    saw_dir: bool,
    limits: ParserLimits,
    failed: bool,
}

impl ListReportParser {
    /// Creates a parser for the listing of `dir` (repository-relative).
    pub fn new(dir: RelPath) -> Self {
        Self {
            dir,
            xml: XmlReader::new(),
            depth: 0,
            item: None,
            in_author: None,
            text: String::new(),
            saw_dir: false,
            limits: ParserLimits::default(),
            failed: false,
        }
    }

    fn finish_item(&mut self, item: Item, out: &mut Vec<ListItem>) -> Result<()> {
        let path = item.path.trim();
        let Some(rel) = path.strip_prefix('/') else {
            return Err(corrupt("list-report item path is not absolute"));
        };
        let entry = |name: String| -> Result<DirEntry> {
            Ok(DirEntry {
                name,
                kind: item.kind,
                size: item.size,
                created_rev: item.rev,
                last_author: item.author.clone(),
                time: item.date.as_deref().map(parse_timestamp).transpose()?,
                has_props: false, // not part of the list-report answer
                lock: None,
            })
        };
        let rel_path = if rel.is_empty() {
            Ok(RelPath::root())
        } else {
            RelPath::from_server(rel)
        };
        match rel_path {
            Ok(p) if p == self.dir => {
                self.saw_dir = true;
                out.push(ListItem::Dir(entry(
                    p.file_name().unwrap_or("").to_owned(),
                )?));
            }
            Ok(p) if p.parent().as_ref() == Some(&self.dir) => {
                out.push(ListItem::Entry(entry(
                    p.file_name().unwrap_or("").to_owned(),
                )?));
            }
            Ok(p) => {
                // Deeper levels of a recursive listing are not direct children; skip them.
                if !p.starts_with(&self.dir) {
                    return Err(corrupt(format!(
                        "list-report item {p} is outside {}",
                        self.dir
                    )));
                }
            }
            Err(error) => out.push(ListItem::Rejected(RejectedEntry {
                href: path.to_owned(),
                error,
            })),
        }
        Ok(())
    }

    fn run(&mut self, events: Vec<XmlEvent>) -> Result<Vec<ListItem>> {
        let mut out = Vec::new();
        for ev in events {
            match ev {
                XmlEvent::Start(tag) => {
                    self.depth += 1;
                    match self.depth {
                        1 if !tag.name.is(&Ns::Svn, "list-report") => {
                            return Err(corrupt("expected an S:list-report document"));
                        }
                        2 if tag.name.is(&Ns::Svn, "item") => {
                            let kind = match tag.attr("node-kind") {
                                Some("file") => NodeKind::File,
                                Some("dir") => NodeKind::Dir,
                                _ => NodeKind::Unknown,
                            };
                            let size = tag
                                .attr("size")
                                .and_then(|s| s.parse::<i64>().ok())
                                .and_then(|n| u64::try_from(n).ok());
                            self.item = Some(Item {
                                kind,
                                size,
                                rev: tag.attr("created-rev").map(parse_revnum).transpose()?,
                                date: tag.attr("date").map(str::to_owned),
                                path: String::new(),
                                author: None,
                            });
                            self.text.clear();
                        }
                        3 if self.item.is_some()
                            && tag.name.is(&Ns::Dav, "creator-displayname") =>
                        {
                            self.in_author = Some(tag.attr("encoding").map(str::to_owned));
                            self.text.clear();
                        }
                        _ => {}
                    }
                }
                XmlEvent::Text(t) => {
                    if self.item.is_some() && (self.depth == 2 || self.in_author.is_some()) {
                        if self.depth == 2 {
                            // Item path text sits directly in `S:item`.
                            if let Some(item) = self.item.as_mut() {
                                item.path.push_str(&t);
                                ParserLimits::check(
                                    item.path.len(),
                                    self.limits.max_href_bytes,
                                    "list-report path",
                                )?;
                            }
                        } else {
                            self.text.push_str(&t);
                            ParserLimits::check(
                                self.text.len(),
                                self.limits.max_href_bytes,
                                "list-report author",
                            )?;
                        }
                    }
                }
                XmlEvent::End(_) => {
                    if self.depth == 3
                        && let Some(enc) = self.in_author.take()
                        && let Some(item) = self.item.as_mut()
                    {
                        item.author = Some(decode_display_field(
                            std::mem::take(&mut self.text),
                            enc.as_deref(),
                        )?);
                    } else if self.depth == 2
                        && let Some(item) = self.item.take()
                    {
                        self.finish_item(item, &mut out)?;
                    }
                    self.depth = self.depth.saturating_sub(1);
                }
            }
        }
        Ok(out)
    }
}

impl ResponseParser for ListReportParser {
    type Item = ListItem;

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<ListItem>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.feed(chunk).and_then(|events| self.run(events));
        self.failed = result.is_err();
        result
    }

    fn finish(&mut self) -> Result<Vec<ListItem>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.finish().and_then(|events| {
            let items = self.run(events)?;
            if self.saw_dir {
                Ok(items)
            } else {
                Err(corrupt("list-report does not include the directory itself"))
            }
        });
        self.failed = result.is_err();
        result
    }
}

/// Parses a complete list-report body.
pub fn parse_list_report(dir: &RelPath, body: &[u8]) -> Result<Vec<ListItem>> {
    parse_chunked(&mut ListReportParser::new(dir.clone()), body, 0)
}

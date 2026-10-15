//! `S:get-location-segments`: the location history of a node (where it lived in which
//! revisions), followed back through its copies. The ancestry check of a switch uses it.

use svn_types::{RelPath, Result, Revnum};

use crate::caps::{UrlBuilder, advertise_caps};
use crate::display_path::RepoDisplayPath;
use crate::http::{HttpRequest, Method, ResponseParser, parse_chunked};
use crate::limits::ParserLimits;
use crate::util::{corrupt, parse_revnum};
use crate::xml::{Ns, XmlEvent, XmlReader, XmlWriter};

/// One segment: the node had `path` (absolute, display only: `/trunk/a`, `/` for the root) in
/// `range_start..=range_end`; `path` is `None` for a gap in its history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationSegment {
    /// Oldest revision of the range.
    pub range_start: Revnum,
    /// Youngest revision of the range.
    pub range_end: Revnum,
    /// Repository path in that range; `None` for a gap.
    pub path: Option<RepoDisplayPath>,
}

/// Builds `REPORT` on the peg-revision URL of `path` (repository-relative) asking for its whole
/// history: from `peg` back to revision 0, like libsvn_ra_serf (the request goes to the node
/// itself, so path-based authorization needs read access to it only).
pub fn location_segments_request(
    urls: &UrlBuilder,
    path: &RelPath,
    peg: Revnum,
) -> Result<HttpRequest> {
    let rev = peg.0.to_string();
    let mut w = XmlWriter::new();
    w.start("S:get-location-segments", &[("xmlns:S", "svn:")])
        .element("S:path", &[], "")
        .element("S:peg-revision", &[], &rev)
        .element("S:start-revision", &[], &rev)
        .element("S:end-revision", &[], "0")
        .end();
    let req = HttpRequest::new(Method::Report, urls.rev_root(peg, path)?).xml_body(w.finish()?);
    Ok(advertise_caps(req))
}

/// Incremental parser emitting one [`LocationSegment`] per `S:location-segment`, youngest first
/// as the server sends them.
#[derive(Default)]
pub struct LocationSegmentsParser {
    xml: XmlReader,
    depth: usize,
    limits: ParserLimits,
    failed: bool,
}

impl LocationSegmentsParser {
    /// Creates a parser with the default limits.
    pub fn new() -> Self {
        Self::default()
    }

    fn run(&mut self, events: Vec<XmlEvent>) -> Result<Vec<LocationSegment>> {
        let mut out = Vec::new();
        for ev in events {
            match ev {
                XmlEvent::Start(tag) => {
                    self.depth += 1;
                    match self.depth {
                        1 if !tag.name.is(&Ns::Svn, "get-location-segments-report") => {
                            return Err(corrupt(
                                "expected an S:get-location-segments-report document",
                            ));
                        }
                        2 if tag.name.is(&Ns::Svn, "location-segment") => {
                            let rev = |name: &str| {
                                tag.attr(name)
                                    .ok_or_else(|| {
                                        corrupt(format!("S:location-segment without {name}"))
                                    })
                                    .and_then(parse_revnum)
                            };
                            let (range_start, range_end) = (rev("range-start")?, rev("range-end")?);
                            if range_start > range_end {
                                return Err(corrupt(format!(
                                    "location segment r{}:{} runs backwards",
                                    range_start.0, range_end.0
                                )));
                            }
                            let path = match tag.attr("path") {
                                Some(p) => {
                                    ParserLimits::check(
                                        p.len(),
                                        self.limits.max_href_bytes,
                                        "location segment path",
                                    )?;
                                    // mod_dav_svn 1.14 and 1.15 send repository-relative
                                    // paths (`trunk/a`, `""` for the root); a leading `/` is
                                    // accepted too.
                                    Some(RepoDisplayPath::parse(&format!(
                                        "/{}",
                                        p.strip_prefix('/').unwrap_or(p)
                                    ))?)
                                }
                                None => None,
                            };
                            out.push(LocationSegment {
                                range_start,
                                range_end,
                                path,
                            });
                        }
                        _ => {}
                    }
                }
                XmlEvent::Text(_) => {}
                XmlEvent::End(_) => self.depth = self.depth.saturating_sub(1),
            }
        }
        Ok(out)
    }
}

impl ResponseParser for LocationSegmentsParser {
    type Item = LocationSegment;

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<LocationSegment>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.feed(chunk).and_then(|events| self.run(events));
        self.failed = result.is_err();
        result
    }

    fn finish(&mut self) -> Result<Vec<LocationSegment>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.finish().and_then(|events| self.run(events));
        self.failed = result.is_err();
        result
    }
}

/// Parses a complete get-location-segments-report body.
pub fn parse_location_segments(body: &[u8]) -> Result<Vec<LocationSegment>> {
    parse_chunked(&mut LocationSegmentsParser::new(), body, 0)
}

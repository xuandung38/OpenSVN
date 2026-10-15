//! `get-location-segments` REPORT: the history of a node, location by location.
//!
//! The answer lists, youngest first, the stretches of revisions in which the node lived at one
//! path (`S:location-segment path="trunk" range-start="3" range-end="7"`), following every copy
//! back to its source; a segment without a path is a gap, revisions in which the node's history
//! does not exist.

use svn_types::{RelPath, Result, Revnum};

use crate::caps::{UrlBuilder, advertise_caps};
use crate::http::{HttpRequest, Method, ResponseParser, parse_chunked};
use crate::util::{corrupt, parse_revnum};
use crate::xml::{Ns, XmlEvent, XmlReader, XmlWriter};

/// Builds `REPORT rvr/PEG/path` with an `S:get-location-segments` body asking for the history
/// of the repository path `path`@`peg` in `end..=start`. Like libsvn_ra_serf the report is
/// addressed to the node itself, so `S:path` is empty.
pub fn location_segments_request(
    urls: &UrlBuilder,
    path: &RelPath,
    peg: Revnum,
    start: Revnum,
    end: Revnum,
) -> Result<HttpRequest> {
    let mut w = XmlWriter::new();
    w.start("S:get-location-segments", &[("xmlns:S", "svn:")])
        .element("S:path", &[], "")
        .element("S:peg-revision", &[], &peg.0.to_string())
        .element("S:start-revision", &[], &start.0.to_string())
        .element("S:end-revision", &[], &end.0.to_string())
        .end();
    let req = HttpRequest::new(Method::Report, urls.rev_root(peg, path)?).xml_body(w.finish()?);
    Ok(advertise_caps(req))
}

/// One `S:location-segment` of the answer (`path="trunk"`, without a leading slash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireSegment {
    /// Oldest revision of the stretch.
    pub range_start: Revnum,
    /// Youngest revision of the stretch.
    pub range_end: Revnum,
    /// Repository-relative path (validated like every server path); `None` for a gap.
    pub path: Option<RelPath>,
}

/// Incremental parser for `S:get-location-segments-report`.
pub struct LocationSegmentsParser {
    xml: XmlReader,
    depth: usize,
    failed: bool,
}

impl Default for LocationSegmentsParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LocationSegmentsParser {
    /// Creates a parser.
    pub fn new() -> Self {
        Self {
            xml: XmlReader::new(),
            depth: 0,
            failed: false,
        }
    }

    fn run(&mut self, events: Vec<XmlEvent>) -> Result<Vec<WireSegment>> {
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
                                        corrupt(format!("location-segment without {name}"))
                                    })
                                    .and_then(parse_revnum)
                            };
                            let (range_start, range_end) = (rev("range-start")?, rev("range-end")?);
                            if range_start > range_end {
                                return Err(corrupt(format!(
                                    "location-segment range {}:{} is reversed",
                                    range_start.0, range_end.0
                                )));
                            }
                            // mod_dav_svn sends the path relative to the repository root
                            // (`trunk`, empty for the root); an absolute form is read the same.
                            let path = match tag.attr("path") {
                                None => None,
                                Some(p) => {
                                    let rel = p.strip_prefix('/').unwrap_or(p);
                                    Some(if rel.is_empty() {
                                        RelPath::root()
                                    } else {
                                        RelPath::from_server(rel)?
                                    })
                                }
                            };
                            out.push(WireSegment {
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
    type Item = WireSegment;

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<WireSegment>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.feed(chunk).and_then(|events| self.run(events));
        self.failed = result.is_err();
        result
    }

    fn finish(&mut self) -> Result<Vec<WireSegment>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.finish().and_then(|events| self.run(events));
        self.failed = result.is_err();
        result
    }
}

/// Parses a complete answer.
pub fn parse_location_segments(body: &[u8]) -> Result<Vec<WireSegment>> {
    parse_chunked(&mut LocationSegmentsParser::new(), body, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<S:get-location-segments-report xmlns:S="svn:" xmlns:D="DAV:">
<S:location-segment path="branches/b &amp; c" range-start="7" range-end="9"/>
<S:location-segment range-start="6" range-end="6"/>
<S:location-segment path="/trunk" range-start="1" range-end="5"/>
<S:location-segment path="" range-start="0" range-end="0"/>
</S:get-location-segments-report>
"#;

    #[test]
    fn parses_segments_and_gaps_in_any_chunking() {
        let want = vec![
            WireSegment {
                range_start: Revnum(7),
                range_end: Revnum(9),
                path: Some(RelPath::new("branches/b & c").unwrap()),
            },
            WireSegment {
                range_start: Revnum(6),
                range_end: Revnum(6),
                path: None,
            },
            WireSegment {
                range_start: Revnum(1),
                range_end: Revnum(5),
                path: Some(RelPath::new("trunk").unwrap()),
            },
            WireSegment {
                range_start: Revnum(0),
                range_end: Revnum(0),
                path: Some(RelPath::root()),
            },
        ];
        for chunk in [0, 1, 7] {
            let got = parse_chunked(&mut LocationSegmentsParser::new(), ANSWER.as_bytes(), chunk)
                .unwrap();
            assert_eq!(got, want, "chunk size {chunk}");
        }
    }

    #[test]
    fn rejects_unsafe_or_malformed_segments() {
        for bad in [
            r#"<S:location-segment path="//trunk" range-start="1" range-end="2"/>"#,
            r#"<S:location-segment path="a/con" range-start="1" range-end="2"/>"#,
            r#"<S:location-segment path="/a/../b" range-start="1" range-end="2"/>"#,
            r#"<S:location-segment path="/a" range-start="3" range-end="2"/>"#,
            r#"<S:location-segment path="/a" range-start="x" range-end="2"/>"#,
            r#"<S:location-segment path="/a" range-end="2"/>"#,
        ] {
            let body = format!(
                r#"<S:get-location-segments-report xmlns:S="svn:">{bad}</S:get-location-segments-report>"#
            );
            assert!(parse_location_segments(body.as_bytes()).is_err(), "{bad}");
        }
        let other = br#"<S:log-report xmlns:S="svn:"/>"#;
        assert!(parse_location_segments(other).is_err());
    }
}

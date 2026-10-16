//! `S:inherited-props-report`: the properties a node inherits from its parents (svn 1.8+
//! servers, capability `inherited-props`).
//!
//! The request goes to the peg-revision URL of the node and names it with an empty `S:path`, as
//! libsvn_ra_serf does. The answer has one `S:iprop-item` per parent that has properties, the
//! repository root first:
//!
//! ```xml
//! <S:inherited-props-report xmlns:S="svn:" xmlns:D="DAV:">
//! <S:iprop-item>
//! <S:iprop-path>trunk</S:iprop-path>
//! <S:iprop-propname>svn:ignore</S:iprop-propname>
//! <S:iprop-propval>*.o</S:iprop-propval>
//! </S:iprop-item>
//! </S:inherited-props-report>
//! ```
//!
//! A value that is not XML-safe comes base64-encoded (`encoding="base64"`).

use svn_types::{RelPath, Result, Revnum};

use crate::caps::{UrlBuilder, advertise_caps};
use crate::http::{HttpRequest, Method, ResponseParser, parse_chunked};
use crate::limits::ParserLimits;
use crate::util::{corrupt, decode_bytes_field, decode_text_field};
use crate::xml::{Ns, XmlEvent, XmlReader, XmlWriter};

/// Properties of one parent: its repository-relative path (`""` for the root) and its
/// properties in the order the server sent them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritedItem {
    /// Repository-relative path of the parent, without a leading `/`.
    pub path: String,
    /// `(name, value)` pairs.
    pub props: Props,
}

/// Builds the `REPORT` for the node at repository path `repo_path` in revision `rev`.
pub fn inherited_props_request(
    urls: &UrlBuilder,
    rev: Revnum,
    repo_path: &RelPath,
) -> Result<HttpRequest> {
    let mut w = XmlWriter::new();
    w.start("S:inherited-props-report", &[("xmlns:S", "svn:")]);
    w.element("S:revision", &[], &rev.0.to_string());
    w.element("S:path", &[], "");
    w.end();
    let req =
        HttpRequest::new(Method::Report, urls.rev_root(rev, repo_path)?).xml_body(w.finish()?);
    Ok(advertise_caps(req))
}

/// `(name, value)` pairs of one item.
type Props = Vec<(String, Vec<u8>)>;

/// Which text element of an item is being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Path,
    Name,
    Value,
}

/// Incremental parser emitting one [`InheritedItem`] per `S:iprop-item`.
#[derive(Default)]
pub struct InheritedPropsParser {
    xml: XmlReader,
    depth: usize,
    /// The item being read: its path (once seen) and properties.
    item: Option<(Option<String>, Props)>,
    name: Option<String>,
    field: Option<(Field, Option<String>)>,
    text: String,
    limits: ParserLimits,
    failed: bool,
}

impl InheritedPropsParser {
    /// Creates a parser with the default limits.
    pub fn new() -> Self {
        Self::default()
    }

    fn run(&mut self, events: Vec<XmlEvent>) -> Result<Vec<InheritedItem>> {
        let mut out = Vec::new();
        for ev in events {
            match ev {
                XmlEvent::Start(tag) => {
                    self.depth += 1;
                    match self.depth {
                        1 if !tag.name.is(&Ns::Svn, "inherited-props-report") => {
                            return Err(corrupt("expected an S:inherited-props-report document"));
                        }
                        2 if tag.name.is(&Ns::Svn, "iprop-item") => {
                            self.item = Some((None, Vec::new()));
                            self.name = None;
                        }
                        3 if self.item.is_some() && tag.name.ns == Ns::Svn => {
                            let field = match tag.name.local.as_str() {
                                "iprop-path" => Some(Field::Path),
                                "iprop-propname" => Some(Field::Name),
                                "iprop-propval" => Some(Field::Value),
                                _ => None,
                            };
                            self.field =
                                field.map(|f| (f, tag.attr("encoding").map(str::to_owned)));
                            self.text.clear();
                        }
                        _ => {}
                    }
                }
                XmlEvent::Text(t) => {
                    if self.depth == 3
                        && let Some((field, _)) = self.field
                    {
                        self.text.push_str(&t);
                        let max = if field == Field::Value {
                            self.limits.max_text_bytes
                        } else {
                            self.limits.max_href_bytes
                        };
                        ParserLimits::check(self.text.len(), max, "inherited property")?;
                    }
                }
                XmlEvent::End(_) => {
                    if self.depth == 3 {
                        self.store_field()?;
                    } else if self.depth == 2
                        && let Some((path, props)) = self.item.take()
                    {
                        if self.name.is_some() {
                            return Err(corrupt("S:iprop-propname without S:iprop-propval"));
                        }
                        let path =
                            path.ok_or_else(|| corrupt("S:iprop-item without S:iprop-path"))?;
                        out.push(InheritedItem { path, props });
                    }
                    self.depth = self.depth.saturating_sub(1);
                }
            }
        }
        Ok(out)
    }

    fn store_field(&mut self) -> Result<()> {
        let (Some((field, encoding)), Some((path, props))) =
            (self.field.take(), self.item.as_mut())
        else {
            return Ok(());
        };
        let raw = std::mem::take(&mut self.text);
        let enc = encoding.as_deref();
        match field {
            Field::Path => {
                let text = decode_text_field(raw, enc)?;
                let rel = text.trim().trim_start_matches('/');
                RelPath::new(rel).map_err(|_| corrupt(format!("bad iprop path {rel:?}")))?;
                *path = Some(rel.to_owned());
            }
            Field::Name => {
                if self.name.is_some() {
                    return Err(corrupt("two S:iprop-propname in a row"));
                }
                let name = decode_text_field(raw, enc)?;
                if name.is_empty() {
                    return Err(corrupt("empty S:iprop-propname"));
                }
                self.name = Some(name);
            }
            Field::Value => {
                let name = self
                    .name
                    .take()
                    .ok_or_else(|| corrupt("S:iprop-propval without S:iprop-propname"))?;
                props.push((name, decode_bytes_field(raw, enc)?));
            }
        }
        Ok(())
    }
}

impl ResponseParser for InheritedPropsParser {
    type Item = InheritedItem;

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<InheritedItem>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.feed(chunk).and_then(|events| self.run(events));
        self.failed = result.is_err();
        result
    }

    fn finish(&mut self) -> Result<Vec<InheritedItem>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.finish().and_then(|events| self.run(events));
        self.failed = result.is_err();
        result
    }
}

/// Parses a complete inherited-props-report body.
pub fn parse_inherited_props(body: &[u8]) -> Result<Vec<InheritedItem>> {
    parse_chunked(&mut InheritedPropsParser::new(), body, 0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use svn_types::ErrorCode;

    const HEAD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<S:inherited-props-report xmlns:S="svn:" xmlns:D="DAV:">"#;

    #[test]
    fn parses_items_root_first() {
        let body = format!(
            "{HEAD}
<S:iprop-item>
<S:iprop-path>/</S:iprop-path>
<S:iprop-propname>svn:global-ignores</S:iprop-propname>
<S:iprop-propval>*.bak
</S:iprop-propval>
</S:iprop-item>
<S:iprop-item>
<S:iprop-path>trunk</S:iprop-path>
<S:iprop-propname>svn:ignore</S:iprop-propname>
<S:iprop-propval>*.o
build
</S:iprop-propval>
<S:iprop-propname>bin</S:iprop-propname>
<S:iprop-propval encoding=\"base64\">AAEC</S:iprop-propval>
</S:iprop-item>
</S:inherited-props-report>"
        );
        let items = parse_inherited_props(body.as_bytes()).unwrap();
        assert_eq!(
            items,
            vec![
                InheritedItem {
                    path: String::new(),
                    props: vec![("svn:global-ignores".into(), b"*.bak\n".to_vec())],
                },
                InheritedItem {
                    path: "trunk".into(),
                    props: vec![
                        ("svn:ignore".into(), b"*.o\nbuild\n".to_vec()),
                        ("bin".into(), vec![0, 1, 2]),
                    ],
                },
            ]
        );
    }

    #[test]
    fn empty_report_and_split_chunks() {
        let body = format!("{HEAD}\n</S:inherited-props-report>\n");
        assert!(parse_inherited_props(body.as_bytes()).unwrap().is_empty());
        let body = format!(
            "{HEAD}<S:iprop-item><S:iprop-path>a/b</S:iprop-path><S:iprop-propname>p</S:iprop-propname><S:iprop-propval>v</S:iprop-propval></S:iprop-item></S:inherited-props-report>"
        );
        let mut parser = InheritedPropsParser::new();
        let mut items = Vec::new();
        for chunk in body.as_bytes().chunks(7) {
            items.extend(parser.feed(chunk).unwrap());
        }
        items.extend(parser.finish().unwrap());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, "a/b");
    }

    #[test]
    fn malformed_reports_are_corrupt() {
        for bad in [
            "<S:log-report xmlns:S=\"svn:\"></S:log-report>".to_owned(),
            format!(
                "{HEAD}<S:iprop-item><S:iprop-propname>p</S:iprop-propname><S:iprop-propval>v</S:iprop-propval></S:iprop-item></S:inherited-props-report>"
            ),
            format!(
                "{HEAD}<S:iprop-item><S:iprop-path>t</S:iprop-path><S:iprop-propval>v</S:iprop-propval></S:iprop-item></S:inherited-props-report>"
            ),
            format!(
                "{HEAD}<S:iprop-item><S:iprop-path>t</S:iprop-path><S:iprop-propname>p</S:iprop-propname></S:iprop-item></S:inherited-props-report>"
            ),
            format!(
                "{HEAD}<S:iprop-item><S:iprop-path>../x</S:iprop-path></S:iprop-item></S:inherited-props-report>"
            ),
            format!(
                "{HEAD}<S:iprop-item><S:iprop-path>t</S:iprop-path><S:iprop-propname>p</S:iprop-propname><S:iprop-propval encoding=\"base64\">!!</S:iprop-propval></S:iprop-item></S:inherited-props-report>"
            ),
        ] {
            let err = parse_inherited_props(bad.as_bytes()).expect_err(&bad);
            assert_eq!(err.code, ErrorCode::Corrupt, "{bad}: {err}");
        }
    }
}

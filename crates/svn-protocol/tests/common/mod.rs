//! Shared helpers for the golden tests: fixture loading and chunked feeding.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use svn_protocol::{Capabilities, Headers, ResponseHead, ResponseParser, UrlBuilder};
use svn_types::RepoUrl;

/// One recorded HTTP exchange (see `tools/capture`).
pub struct Fixture {
    pub method: String,
    pub path: String,
    pub status: u16,
    pub req_headers: Headers,
    pub resp_headers: Headers,
    pub req_body: Vec<u8>,
    pub resp_body: Vec<u8>,
}

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// Loads `tests/data/<name>.{meta,req,resp}`.
pub fn load(name: &str) -> Fixture {
    let base = data_dir().join(name);
    let meta = std::fs::read_to_string(base.with_extension("meta"))
        .unwrap_or_else(|e| panic!("fixture {name}: {e}"));
    let (mut method, mut path, mut status) = (String::new(), String::new(), 0);
    let (mut req_headers, mut resp_headers) = (Headers::new(), Headers::new());
    let mut in_response = false;
    for line in meta.lines() {
        if let Some(rest) = line.strip_prefix("REQUEST ") {
            let (m, p) = rest.split_once(' ').unwrap();
            method = m.to_owned();
            path = p.to_owned();
        } else if let Some(rest) = line.strip_prefix("RESPONSE ") {
            status = rest.trim().parse().unwrap();
            in_response = true;
        } else if let Some(rest) = line.strip_prefix("> ") {
            let (k, v) = rest.split_once(": ").unwrap();
            req_headers.push(k, v);
        } else if let Some(rest) = line.strip_prefix("< ") {
            let (k, v) = rest.split_once(": ").unwrap();
            assert!(in_response);
            resp_headers.push(k, v);
        }
    }
    let read = |ext: &str| std::fs::read(base.with_extension(ext)).unwrap_or_default();
    Fixture {
        method,
        path,
        status,
        req_headers,
        resp_headers,
        req_body: read("req"),
        resp_body: read("resp"),
    }
}

impl Fixture {
    pub fn head(&self) -> ResponseHead {
        ResponseHead::new(self.status, self.resp_headers.clone())
    }
}

/// Raw bytes of an optional companion file such as `<name>.svn.xml`.
pub fn companion(name: &str, ext: &str) -> String {
    let p = data_dir().join(format!("{name}.{ext}"));
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("companion {name}.{ext}: {e}"))
}

pub fn session_url() -> RepoUrl {
    RepoUrl::parse("http://svn.test.local/svn/repo").unwrap()
}

pub fn caps() -> Capabilities {
    let f = load("caps/options-200");
    Capabilities::from_options_response(&session_url(), &f.head(), &f.resp_body).unwrap()
}

pub fn urls() -> UrlBuilder {
    UrlBuilder::new(&session_url(), &caps()).unwrap()
}

/// Feeds `body` to a fresh parser in pieces cut at `cuts` (byte offsets) and collects items.
pub fn run_split<P: ResponseParser>(mut parser: P, body: &[u8], cuts: &[usize]) -> Vec<P::Item> {
    let mut points: Vec<usize> = cuts.iter().map(|c| c % (body.len() + 1)).collect();
    points.sort_unstable();
    points.dedup();
    let mut items = Vec::new();
    let mut last = 0;
    for p in points {
        items.extend(parser.feed(&body[last..p]).unwrap());
        last = p;
    }
    items.extend(parser.feed(&body[last..]).unwrap());
    items.extend(parser.finish().unwrap());
    items
}

/// `(action, kind, path, copyfrom-path, copyfrom-rev)` of one `<path>` element.
pub type SvnXmlPath = (String, String, String, Option<String>, Option<String>);

/// One `<logentry>` of `svn log --xml`, flattened in document (pre-)order.
#[derive(Debug, PartialEq, Eq)]
pub struct SvnXmlLogRow {
    pub rev: u64,
    pub depth: u32,
    pub author: Option<String>,
    pub date: Option<String>,
    pub msg: Option<String>,
    /// `(action, kind, path, copyfrom-path, copyfrom-rev)`
    pub paths: Vec<SvnXmlPath>,
}

/// Reads the output of `svn log --xml` using the crate's own XML reader.
pub fn svn_log_xml(xml: &str) -> Vec<SvnXmlLogRow> {
    use svn_protocol::xml::{XmlEvent, read_all};
    let mut rows: Vec<SvnXmlLogRow> = Vec::new();
    let mut open: Vec<usize> = Vec::new(); // indexes of rows whose <logentry> is open
    let mut field: Option<String> = None;
    let mut text = String::new();
    let mut path_attrs: Option<svn_protocol::xml::StartTag> = None;
    for ev in read_all(xml.as_bytes()).unwrap() {
        match ev {
            XmlEvent::Start(t) => {
                let name = t.name.local.clone();
                if name == "logentry" {
                    rows.push(SvnXmlLogRow {
                        rev: t.attr("revision").unwrap().parse().unwrap(),
                        depth: u32::try_from(open.len()).unwrap(),
                        author: None,
                        date: None,
                        msg: None,
                        paths: Vec::new(),
                    });
                    open.push(rows.len() - 1);
                } else if matches!(name.as_str(), "author" | "date" | "msg" | "path") {
                    text.clear();
                    if name == "path" {
                        path_attrs = Some(t);
                    }
                    field = Some(name);
                }
            }
            XmlEvent::Text(t) => {
                if field.is_some() {
                    text.push_str(&t);
                }
            }
            XmlEvent::End(n) => {
                let Some(&idx) = open.last() else { continue };
                match (n.local.as_str(), field.take()) {
                    ("author", Some(_)) => rows[idx].author = Some(text.clone()),
                    ("date", Some(_)) => rows[idx].date = Some(text.clone()),
                    ("msg", Some(_)) => rows[idx].msg = Some(text.clone()),
                    ("path", Some(_)) => {
                        let a = path_attrs.take().unwrap();
                        rows[idx].paths.push((
                            a.attr("action").unwrap().to_owned(),
                            a.attr("kind").unwrap().to_owned(),
                            text.clone(),
                            a.attr("copyfrom-path").map(str::to_owned),
                            a.attr("copyfrom-rev").map(str::to_owned),
                        ));
                    }
                    ("logentry", _) => {
                        open.pop();
                    }
                    _ => {}
                }
            }
        }
    }
    rows
}

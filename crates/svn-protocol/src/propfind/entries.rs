//! `PROPFIND` results as [`DirEntry`] values.

use svn_types::{DirEntry, ErrorCode, NodeKind, RelPath, Result, SvnError};

use super::multistatus::{MultiStatusParser, PropfindEntry};
use crate::caps::{HrefKind, UrlBuilder};
use crate::http::{ResponseParser, parse_chunked};
use crate::util::{corrupt, parse_revnum, parse_timestamp};
use crate::xml::Ns;

/// Converts one multistatus response into the node path and its [`DirEntry`].
///
/// The href must be a revision-root or public URL under the session's repository; the path goes
/// through `RelPath::from_server`. `DirEntry::name` is the last path segment (empty for the root).
pub fn dir_entry_from(urls: &UrlBuilder, entry: &PropfindEntry) -> Result<(RelPath, DirEntry)> {
    let path = match urls.accept_href(&entry.href)?.kind {
        HrefKind::RevRoot { path, .. } | HrefKind::Public { path } => path,
        other => {
            return Err(SvnError::new(
                ErrorCode::UnsafePath,
                format!("PROPFIND response names an unexpected resource: {other:?}"),
            ));
        }
    };
    let is_dir = entry
        .get(&Ns::Dav, "resourcetype")
        .is_some_and(|v| v.child_names.iter().any(|c| c == "collection"));
    let text = |local: &str| {
        entry
            .get(&Ns::Dav, local)
            .map(|v| v.text().to_owned())
            .filter(|t| !t.is_empty())
    };
    let size = if is_dir {
        None
    } else {
        text("getcontentlength")
            .map(|s| {
                s.parse::<u64>()
                    .map_err(|_| corrupt(format!("invalid getcontentlength {s:?}")))
            })
            .transpose()?
    };
    let has_props = entry
        .get(&Ns::SvnDav, "deadprop-count")
        .map(|v| {
            v.text()
                .parse::<u64>()
                .map(|n| n > 0)
                .map_err(|_| corrupt("invalid deadprop-count"))
        })
        .transpose()?
        .unwrap_or(false);
    let dir_entry = DirEntry {
        name: path.file_name().unwrap_or("").to_owned(),
        kind: if is_dir {
            NodeKind::Dir
        } else {
            NodeKind::File
        },
        size,
        created_rev: text("version-name").map(|v| parse_revnum(&v)).transpose()?,
        last_author: text("creator-displayname"),
        time: text("creationdate")
            .map(|v| parse_timestamp(&v))
            .transpose()?,
        has_props,
        lock: None,
    };
    Ok((path, dir_entry))
}

fn run_to_end(body: &[u8]) -> Result<Vec<PropfindEntry>> {
    let mut parser = MultiStatusParser::new();
    parse_chunked(&mut parser, body, 0)
}

/// Parses the body of a `Depth: 0` entry PROPFIND (stat). The first response is the node.
pub fn parse_stat(urls: &UrlBuilder, body: &[u8]) -> Result<DirEntry> {
    let entries = run_to_end(body)?;
    let first = entries
        .first()
        .ok_or_else(|| corrupt("PROPFIND answered without a response"))?;
    dir_entry_from(urls, first).map(|(_, e)| e)
}

/// An entry the parser refused, kept so the caller can surface it instead of losing it silently.
#[derive(Debug, Clone)]
pub struct RejectedEntry {
    /// Href as sent by the server.
    pub href: String,
    /// Why it was refused (`UnsafePath` for hostile or Windows-reserved names).
    pub error: SvnError,
}

/// Item produced by [`ListParser`].
#[derive(Debug, Clone)]
pub enum ListItem {
    /// The listed directory itself.
    Dir(DirEntry),
    /// A direct child.
    Entry(DirEntry),
    /// A child that failed validation.
    Rejected(RejectedEntry),
}

/// Streaming parser for a `Depth: 1` listing of `target`.
pub struct ListParser<'a> {
    urls: &'a UrlBuilder,
    target: RelPath,
    inner: MultiStatusParser,
    saw_target: bool,
}

impl<'a> ListParser<'a> {
    /// Creates a parser for the listing of `target`.
    pub fn new(urls: &'a UrlBuilder, target: RelPath) -> Self {
        Self {
            urls,
            target,
            inner: MultiStatusParser::new(),
            saw_target: false,
        }
    }

    fn convert(&mut self, entries: Vec<PropfindEntry>) -> Result<Vec<ListItem>> {
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            match dir_entry_from(self.urls, &e) {
                Ok((path, entry)) if path == self.target => {
                    self.saw_target = true;
                    out.push(ListItem::Dir(entry));
                }
                Ok((path, entry)) if path.parent().as_ref() == Some(&self.target) => {
                    out.push(ListItem::Entry(entry))
                }
                Ok((path, _)) => {
                    return Err(corrupt(format!(
                        "listing of {:?} contains unrelated path {path}",
                        self.target.as_str()
                    )));
                }
                Err(error) => out.push(ListItem::Rejected(RejectedEntry {
                    href: e.href,
                    error,
                })),
            }
        }
        Ok(out)
    }
}

impl ResponseParser for ListParser<'_> {
    type Item = ListItem;

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<ListItem>> {
        let entries = self.inner.feed(chunk)?;
        self.convert(entries)
    }

    fn finish(&mut self) -> Result<Vec<ListItem>> {
        let entries = self.inner.finish()?;
        let items = self.convert(entries)?;
        if !self.saw_target {
            return Err(corrupt("listing does not include the directory itself"));
        }
        Ok(items)
    }
}

/// Collects a whole listing: `(directory, children, rejected)`.
pub fn parse_list(
    urls: &UrlBuilder,
    target: &RelPath,
    body: &[u8],
) -> Result<(DirEntry, Vec<DirEntry>, Vec<RejectedEntry>)> {
    let mut parser = ListParser::new(urls, target.clone());
    let mut dir = None;
    let (mut children, mut rejected) = (Vec::new(), Vec::new());
    for item in parse_chunked(&mut parser, body, 0)? {
        match item {
            ListItem::Dir(d) => dir = Some(d),
            ListItem::Entry(e) => children.push(e),
            ListItem::Rejected(r) => rejected.push(r),
        }
    }
    let dir = dir.ok_or_else(|| corrupt("listing does not include the directory itself"))?;
    Ok((dir, children, rejected))
}

//! Streaming `S:log-report` response parser.

use svn_types::{ChangeAction, LogEntry, NodeKind, Result, Revnum};

use crate::display_path::{DisplayChangedPath, RepoDisplayPath};

use crate::http::{ResponseParser, parse_chunked};
use crate::util::{corrupt, parse_revnum, parse_timestamp};
use crate::xml::{Ns, StartTag, XmlEvent, XmlReader};

/// A log entry with its position in the merge hierarchy.
///
/// With `include-merged-revisions` the server sends the merged-in revisions right after the
/// entry that has `has_children`, then an end marker. `merge_depth` is 0 for top-level entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogItem {
    /// The entry. Its `changed_paths` is always empty: paths are in [`LogItem::changed_paths`]
    /// as display-only values; use [`LogItem::into_log_entry`] to build the contract form.
    pub entry: LogEntry,
    /// Changed paths (when requested), display-only.
    pub changed_paths: Vec<DisplayChangedPath>,
    /// Nesting level in the merge hierarchy.
    pub merge_depth: u32,
    /// The merge was subtractive (a reverse merge).
    pub subtractive_merge: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Rev,
    Author,
    Date,
    Comment,
    Path,
}

struct Building {
    rev: Option<Revnum>,
    end_marker: bool,
    entry: LogEntry,
    paths: Vec<DisplayChangedPath>,
    subtractive: bool,
}

impl LogItem {
    /// Builds the contract `LogEntry` with `changed_paths` filled in. Paths become plain
    /// strings here, so call it only where the contract requires that.
    pub fn into_log_entry(self) -> LogEntry {
        let mut entry = self.entry;
        entry.changed_paths = self
            .changed_paths
            .into_iter()
            .map(DisplayChangedPath::into_contract)
            .collect();
        entry
    }
}

/// Incremental parser emitting one [`LogItem`] per `S:log-item`.
pub struct LogParser {
    xml: XmlReader,
    depth: usize,
    merge_depth: u32,
    item: Option<Building>,
    field: Option<Field>,
    text: String,
    path_tag: Option<StartTag>,
    failed: bool,
}

impl Default for LogParser {
    fn default() -> Self {
        Self::new()
    }
}

fn action_for(local: &str) -> Option<ChangeAction> {
    Some(match local {
        "added-path" => ChangeAction::Added,
        "modified-path" => ChangeAction::Modified,
        "deleted-path" => ChangeAction::Deleted,
        "replaced-path" => ChangeAction::Replaced,
        _ => return None,
    })
}

fn kind_for(value: Option<&str>) -> NodeKind {
    match value {
        Some("file") => NodeKind::File,
        Some("dir") => NodeKind::Dir,
        Some("none") => NodeKind::None,
        _ => NodeKind::Unknown,
    }
}

impl LogParser {
    /// Creates a parser.
    pub fn new() -> Self {
        Self {
            xml: XmlReader::new(),
            depth: 0,
            merge_depth: 0,
            item: None,
            field: None,
            text: String::new(),
            path_tag: None,
            failed: false,
        }
    }

    fn start(&mut self, tag: &StartTag) -> Result<()> {
        self.depth += 1;
        match self.depth {
            1 if !tag.name.is(&Ns::Svn, "log-report") => {
                return Err(corrupt("expected an S:log-report document"));
            }
            2 if tag.name.is(&Ns::Svn, "log-item") => {
                self.item = Some(Building {
                    rev: None,
                    end_marker: false,
                    subtractive: false,
                    paths: Vec::new(),
                    entry: LogEntry {
                        rev: Revnum(0),
                        author: None,
                        date: None,
                        message: None,
                        changed_paths: Vec::new(),
                        has_children: false,
                    },
                });
            }
            3 if self.item.is_some() => self.start_field(tag),
            _ => {}
        }
        Ok(())
    }

    fn start_field(&mut self, tag: &StartTag) {
        let n = &tag.name;
        self.text.clear();
        self.field = if n.is(&Ns::Dav, "version-name") {
            Some(Field::Rev)
        } else if n.is(&Ns::Dav, "creator-displayname") {
            Some(Field::Author)
        } else if n.is(&Ns::Svn, "date") {
            Some(Field::Date)
        } else if n.is(&Ns::Dav, "comment") {
            Some(Field::Comment)
        } else if n.ns == Ns::Svn && action_for(&n.local).is_some() {
            self.path_tag = Some(tag.clone());
            Some(Field::Path)
        } else {
            if let Some(item) = self.item.as_mut() {
                if n.is(&Ns::Svn, "has-children") {
                    item.entry.has_children = true;
                } else if n.is(&Ns::Svn, "subtractive-merge") {
                    item.subtractive = true;
                }
            }
            None
        };
    }

    fn finish_field(&mut self) -> Result<()> {
        let (Some(field), Some(item)) = (self.field.take(), self.item.as_mut()) else {
            return Ok(());
        };
        let text = std::mem::take(&mut self.text);
        match field {
            Field::Rev => {
                if text.trim() == "-1" {
                    item.end_marker = true;
                } else {
                    item.rev = Some(parse_revnum(&text)?);
                }
            }
            Field::Author => item.entry.author = Some(text),
            Field::Date => item.entry.date = Some(parse_timestamp(&text)?),
            Field::Comment => item.entry.message = Some(text),
            Field::Path => {
                let tag = self
                    .path_tag
                    .take()
                    .ok_or_else(|| corrupt("internal parser state"))?;
                let action =
                    action_for(&tag.name.local).ok_or_else(|| corrupt("internal parser state"))?;
                let copy_from = match (tag.attr("copyfrom-path"), tag.attr("copyfrom-rev")) {
                    (Some(p), Some(r)) => Some((RepoDisplayPath::parse(p)?, parse_revnum(r)?)),
                    _ => None,
                };
                item.paths.push(DisplayChangedPath {
                    path: RepoDisplayPath::parse(text.trim())?,
                    action,
                    kind: kind_for(tag.attr("node-kind")),
                    copy_from,
                    text_mods: tag.attr("text-mods") != Some("false"),
                    prop_mods: tag.attr("prop-mods") == Some("true"),
                });
            }
        }
        Ok(())
    }

    fn end(&mut self, out: &mut Vec<LogItem>) -> Result<()> {
        if self.depth == 3 {
            self.finish_field()?;
        } else if self.depth == 2
            && let Some(mut item) = self.item.take()
        {
            if item.end_marker {
                self.merge_depth = self.merge_depth.saturating_sub(1);
            } else {
                item.entry.rev = item
                    .rev
                    .ok_or_else(|| corrupt("log item without version-name"))?;
                let merge_depth = self.merge_depth;
                if item.entry.has_children {
                    self.merge_depth += 1;
                }
                out.push(LogItem {
                    entry: item.entry,
                    changed_paths: item.paths,
                    merge_depth,
                    subtractive_merge: item.subtractive,
                });
            }
        }
        self.depth = self.depth.saturating_sub(1);
        Ok(())
    }

    fn run(&mut self, events: Vec<XmlEvent>) -> Result<Vec<LogItem>> {
        let mut out = Vec::new();
        for ev in events {
            match ev {
                XmlEvent::Start(tag) => self.start(&tag)?,
                XmlEvent::Text(t) => {
                    if self.depth == 3 && self.field.is_some() {
                        self.text.push_str(&t);
                    }
                }
                XmlEvent::End(_) => self.end(&mut out)?,
            }
        }
        Ok(out)
    }
}

impl ResponseParser for LogParser {
    type Item = LogItem;

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<LogItem>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.feed(chunk).and_then(|events| self.run(events));
        self.failed = result.is_err();
        result
    }

    fn finish(&mut self) -> Result<Vec<LogItem>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.finish().and_then(|events| self.run(events));
        self.failed = result.is_err();
        result
    }
}

/// Parses a complete log-report body.
pub fn parse_log(body: &[u8]) -> Result<Vec<LogItem>> {
    parse_chunked(&mut LogParser::new(), body, 0)
}

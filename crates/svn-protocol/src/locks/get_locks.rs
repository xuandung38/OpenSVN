//! `S:get-locks-report`: all locks at or below a path.

use svn_types::{Depth, ErrorCode, RelPath, Result, SvnError};

use crate::caps::{UrlBuilder, advertise_caps};
use crate::display_path::{RepoDisplayPath, RepoLock};
use crate::http::{HttpRequest, Method, ResponseParser, parse_chunked};
use crate::limits::ParserLimits;
use crate::util::{corrupt, decode_text_field, parse_timestamp, validate_lock_token};
use crate::xml::{Ns, XmlEvent, XmlReader, XmlWriter};

/// Builds `REPORT` on the public URL of `path`. `depth` limits how far below `path` locks are
/// returned (`Empty`, `Files`, `Immediates` or `Infinity`).
pub fn get_locks_request(urls: &UrlBuilder, path: &RelPath, depth: Depth) -> Result<HttpRequest> {
    let word = match depth {
        Depth::Empty => "empty",
        Depth::Files => "files",
        Depth::Immediates => "immediates",
        Depth::Infinity => "infinity",
        Depth::Exclude | Depth::Unknown => {
            return Err(SvnError::new(
                ErrorCode::Internal,
                "get-locks-report needs a concrete depth",
            ));
        }
    };
    let mut w = XmlWriter::new();
    w.start(
        "S:get-locks-report",
        &[("xmlns:S", "svn:"), ("depth", word)],
    )
    .end();
    let req = HttpRequest::new(Method::Report, urls.public(path)?).xml_body(w.finish()?);
    Ok(advertise_caps(req))
}

#[derive(Default)]
struct Building {
    path: Option<String>,
    token: Option<String>,
    owner: Option<String>,
    comment: Option<String>,
    created: Option<String>,
    expires: Option<String>,
}

/// Fields whose text is kept; text of every other element is dropped without buffering.
const FIELDS: [&str; 6] = [
    "path",
    "token",
    "owner",
    "comment",
    "creationdate",
    "expirationdate",
];

/// Incremental parser emitting one [`RepoLock`] per `S:lock`.
#[derive(Default)]
pub struct GetLocksParser {
    xml: XmlReader,
    depth: usize,
    lock: Option<Building>,
    field: Option<(&'static str, Option<String>)>,
    text: String,
    limits: ParserLimits,
    failed: bool,
}

impl GetLocksParser {
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

    fn run(&mut self, events: Vec<XmlEvent>) -> Result<Vec<RepoLock>> {
        let mut out = Vec::new();
        for ev in events {
            match ev {
                XmlEvent::Start(tag) => {
                    self.depth += 1;
                    match self.depth {
                        1 if !tag.name.is(&Ns::Svn, "get-locks-report") => {
                            return Err(corrupt("expected an S:get-locks-report document"));
                        }
                        2 if tag.name.is(&Ns::Svn, "lock") => self.lock = Some(Building::default()),
                        3 if self.lock.is_some() && tag.name.ns == Ns::Svn => {
                            self.field = FIELDS
                                .iter()
                                .find(|f| **f == tag.name.local)
                                .map(|f| (*f, tag.attr("encoding").map(str::to_owned)));
                            self.text.clear();
                        }
                        _ => {}
                    }
                }
                XmlEvent::Text(t) => {
                    if self.depth == 3
                        && let Some((name, _)) = &self.field
                    {
                        self.text.push_str(&t);
                        let max = if matches!(*name, "comment" | "owner") {
                            self.limits.max_text_bytes
                        } else {
                            self.limits.max_href_bytes
                        };
                        ParserLimits::check(self.text.len(), max, "lock field")?;
                    }
                }
                XmlEvent::End(_) => {
                    if self.depth == 3 {
                        self.store_field()?;
                    } else if self.depth == 2
                        && let Some(b) = self.lock.take()
                    {
                        out.push(finish_lock(b)?);
                    }
                    self.depth = self.depth.saturating_sub(1);
                }
            }
        }
        Ok(out)
    }

    fn store_field(&mut self) -> Result<()> {
        let (Some((field, encoding)), Some(lock)) = (self.field.take(), self.lock.as_mut()) else {
            return Ok(());
        };
        let text = decode_text_field(std::mem::take(&mut self.text), encoding.as_deref())?;
        match field {
            "path" => lock.path = Some(text.trim().to_owned()),
            "token" => lock.token = Some(text.trim().to_owned()),
            "owner" => lock.owner = Some(text.trim().to_owned()),
            "comment" => lock.comment = Some(text),
            "creationdate" => lock.created = Some(text),
            _ => lock.expires = Some(text),
        }
        Ok(())
    }
}

fn finish_lock(b: Building) -> Result<RepoLock> {
    let need = |v: Option<String>, what: &str| {
        v.filter(|s| !s.is_empty())
            .ok_or_else(|| corrupt(format!("S:lock without {what}")))
    };
    Ok(RepoLock {
        path: RepoDisplayPath::parse(&need(b.path, "S:path")?)?,
        token: validate_lock_token(&need(b.token, "S:token")?)?,
        owner: need(b.owner, "S:owner")?,
        comment: b.comment.filter(|c| !c.is_empty()),
        created: parse_timestamp(&need(b.created, "S:creationdate")?)?,
        expires: b
            .expires
            .filter(|e| !e.trim().is_empty())
            .map(|e| parse_timestamp(&e))
            .transpose()?,
    })
}

impl ResponseParser for GetLocksParser {
    type Item = RepoLock;

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<RepoLock>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.feed(chunk).and_then(|events| self.run(events));
        self.failed = result.is_err();
        result
    }

    fn finish(&mut self) -> Result<Vec<RepoLock>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self.xml.finish().and_then(|events| self.run(events));
        self.failed = result.is_err();
        result
    }
}

/// Parses a complete get-locks-report body.
pub fn parse_get_locks(body: &[u8]) -> Result<Vec<RepoLock>> {
    parse_chunked(&mut GetLocksParser::new(), body, 0)
}

//! `S:get-locks-report`: all locks at or below a path.

use svn_types::{Depth, ErrorCode, LockInfo, RelPath, Result, SvnError};

use crate::caps::{UrlBuilder, advertise_caps};
use crate::http::{HttpRequest, Method, ResponseParser, parse_chunked};
use crate::util::{corrupt, parse_timestamp, validate_display_path};
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

/// Incremental parser emitting one [`LockInfo`] per `S:lock`.
#[derive(Default)]
pub struct GetLocksParser {
    xml: XmlReader,
    depth: usize,
    lock: Option<Building>,
    field: Option<String>,
    text: String,
}

impl GetLocksParser {
    /// Creates a parser.
    pub fn new() -> Self {
        Self::default()
    }

    fn run(&mut self, events: Vec<XmlEvent>) -> Result<Vec<LockInfo>> {
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
                            self.field = Some(tag.name.local.clone());
                            self.text.clear();
                        }
                        _ => {}
                    }
                }
                XmlEvent::Text(t) => {
                    if self.depth == 3 && self.field.is_some() {
                        self.text.push_str(&t);
                    }
                }
                XmlEvent::End(_) => {
                    if self.depth == 3 {
                        self.store_field();
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

    fn store_field(&mut self) {
        let (Some(field), Some(lock)) = (self.field.take(), self.lock.as_mut()) else {
            return;
        };
        let text = std::mem::take(&mut self.text);
        match field.as_str() {
            "path" => lock.path = Some(text.trim().to_owned()),
            "token" => lock.token = Some(text.trim().to_owned()),
            "owner" => lock.owner = Some(text.trim().to_owned()),
            "comment" => lock.comment = Some(text),
            "creationdate" => lock.created = Some(text),
            "expirationdate" => lock.expires = Some(text),
            _ => {}
        }
    }
}

fn finish_lock(b: Building) -> Result<LockInfo> {
    let need = |v: Option<String>, what: &str| {
        v.filter(|s| !s.is_empty())
            .ok_or_else(|| corrupt(format!("S:lock without {what}")))
    };
    Ok(LockInfo {
        path: validate_display_path(&need(b.path, "S:path")?)?,
        token: need(b.token, "S:token")?,
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
    type Item = LockInfo;

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<LockInfo>> {
        let events = self.xml.feed(chunk)?;
        self.run(events)
    }

    fn finish(&mut self) -> Result<Vec<LockInfo>> {
        let events = self.xml.finish()?;
        self.run(events)
    }
}

/// Parses a complete get-locks-report body.
pub fn parse_get_locks(body: &[u8]) -> Result<Vec<LockInfo>> {
    parse_chunked(&mut GetLocksParser::new(), body, 0)
}

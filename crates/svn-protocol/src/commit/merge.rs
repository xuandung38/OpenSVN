//! Responses of the commit sequence: transaction creation and `MERGE`.

use svn_types::{ErrorCode, Result, Revnum, SvnError, Timestamp};

use crate::caps::TxnName;
use crate::errors::error_from_response;
use crate::errors::sanitize_message;
use crate::http::{ResponseHead, ResponseParser, parse_chunked};
use crate::limits::ParserLimits;
use crate::util::{corrupt, parse_revnum, parse_timestamp};
use crate::xml::{Ns, XmlEvent, XmlReader};

/// Reads the transaction name from the answer to `POST !svn/me`.
///
/// The name is validated like any transaction name before it is used to build URLs.
pub fn parse_create_txn_response(head: &ResponseHead, body: &[u8]) -> Result<TxnName> {
    if !(200..300).contains(&head.status) {
        return Err(error_from_response(head.status, &head.headers, body));
    }
    let (name, virtual_) = match (
        head.headers.get("SVN-Txn-Name"),
        head.headers.get("SVN-VTxn-Name"),
    ) {
        (Some(n), _) => (Some(n), false),
        (None, v) => (v, true),
    };
    let name = name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| corrupt("create-txn answered without SVN-Txn-Name"))?;
    if name.starts_with('.')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(SvnError::new(
            ErrorCode::UnsafePath,
            format!("invalid transaction name {name:?}"),
        ));
    }
    Ok(TxnName {
        name: name.to_owned(),
        virtual_,
    })
}

/// Outcome of a successful `MERGE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResponse {
    /// The new revision.
    pub new_rev: Revnum,
    /// Commit time, when the server reported it.
    pub date: Option<Timestamp>,
    /// Committer, when the server reported it.
    pub author: Option<String>,
    /// Message of a failed post-commit hook. The commit **succeeded**: show a warning, never retry.
    pub post_commit_error: Option<String>,
}

#[derive(Default)]
struct Resp {
    baseline: bool,
    rev: Option<Revnum>,
    date: Option<Timestamp>,
    author: Option<String>,
}

/// Parser for `D:merge-response`. It yields one [`MergeResponse`] when the document ends.
#[derive(Default)]
pub struct MergeResponseParser {
    xml: XmlReader,
    depth: usize,
    current: Resp,
    best: Option<Resp>,
    field: Option<String>,
    text: String,
    post_commit: Option<String>,
    root_seen: bool,
    limits: ParserLimits,
    failed: bool,
}

impl MergeResponseParser {
    /// Creates a parser.
    pub fn new() -> Self {
        Self::default()
    }

    fn run(&mut self, events: Vec<XmlEvent>) -> Result<()> {
        for ev in events {
            match ev {
                XmlEvent::Start(tag) => {
                    self.depth += 1;
                    if self.depth == 1 {
                        if !tag.name.is(&Ns::Dav, "merge-response") {
                            return Err(corrupt("expected a D:merge-response document"));
                        }
                        self.root_seen = true;
                    } else if tag.name.is(&Ns::Dav, "response") {
                        self.current = Resp::default();
                    } else if tag.name.is(&Ns::Dav, "baseline") {
                        self.current.baseline = true;
                    } else if tag.name.ns == Ns::Dav
                        && matches!(
                            tag.name.local.as_str(),
                            "version-name" | "creationdate" | "creator-displayname"
                        )
                        || tag.name.is(&Ns::Svn, "post-commit-err")
                    {
                        self.field = Some(tag.name.local.clone());
                        self.text.clear();
                    }
                }
                XmlEvent::Text(t) => {
                    if self.field.is_some() {
                        self.text.push_str(&t);
                        ParserLimits::check(
                            self.text.len(),
                            self.limits.max_text_bytes,
                            "merge-response field",
                        )?;
                    }
                }
                XmlEvent::End(name) => {
                    self.depth = self.depth.saturating_sub(1);
                    if let Some(field) = self.field.take() {
                        let text = std::mem::take(&mut self.text);
                        match field.as_str() {
                            "version-name" => self.current.rev = Some(parse_revnum(&text)?),
                            "creationdate" => self.current.date = Some(parse_timestamp(&text)?),
                            "creator-displayname" => self.current.author = Some(text),
                            _ => {
                                self.post_commit = Some(sanitize_message(
                                    text.trim(),
                                    self.limits.max_error_message_bytes,
                                ));
                            }
                        }
                    } else if name.is(&Ns::Dav, "response") {
                        let done = std::mem::take(&mut self.current);
                        let better = match &self.best {
                            None => true,
                            Some(b) => {
                                (done.baseline && !b.baseline)
                                    || (done.baseline == b.baseline && done.rev > b.rev)
                            }
                        };
                        if done.rev.is_some() && better {
                            self.best = Some(done);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn result(&mut self) -> Result<MergeResponse> {
        let best = self
            .best
            .take()
            .ok_or_else(|| corrupt("merge-response without a new revision"))?;
        Ok(MergeResponse {
            new_rev: best
                .rev
                .ok_or_else(|| corrupt("merge-response without a new revision"))?,
            date: best.date,
            author: best.author,
            post_commit_error: self.post_commit.take().filter(|m| !m.is_empty()),
        })
    }
}

impl ResponseParser for MergeResponseParser {
    type Item = MergeResponse;

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<MergeResponse>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self
            .xml
            .feed(chunk)
            .and_then(|events| self.run(events))
            .map(|()| Vec::new());
        self.failed = result.is_err();
        result
    }

    fn finish(&mut self) -> Result<Vec<MergeResponse>> {
        if self.failed {
            return Err(corrupt("parser used after an error"));
        }
        let result = self
            .xml
            .finish()
            .and_then(|events| self.run(events))
            .and_then(|()| self.result())
            .map(|r| vec![r]);
        self.failed = result.is_err();
        result
    }
}

/// Interprets the answer to `MERGE`: the new revision, or the error the server reported.
pub fn parse_merge_response(head: &ResponseHead, body: &[u8]) -> Result<MergeResponse> {
    if !(200..300).contains(&head.status) {
        return Err(error_from_response(head.status, &head.headers, body));
    }
    let mut items = parse_chunked(&mut MergeResponseParser::new(), body, 0)?;
    items
        .pop()
        .ok_or_else(|| corrupt("merge-response without a new revision"))
}

//! `LOCK` response and get-lock `PROPFIND` response parsing.

use svn_types::{RelPath, Result, Timestamp};

use crate::display_path::{RepoDisplayPath, RepoLock};
use crate::errors::error_from_response;
use crate::http::{ResponseHead, parse_chunked};
use crate::limits::ParserLimits;
use crate::propfind::MultiStatusParser;
use crate::util::{corrupt, parse_timestamp, validate_lock_token};
use crate::xml::{Ns, XmlEvent, XmlReader};

/// Element paths below `D:prop`/`D:lockdiscovery` that carry lock data. Matching the whole path
/// keeps an `href` inside a DAV-style comment (`<D:owner><D:href>mailto:..`) from being taken
/// for the lock token.
const TOKEN_PATH: &str = "activelock/locktoken/href";
const OWNER_PATH: &str = "activelock/owner";
const TIMEOUT_PATH: &str = "activelock/timeout";

/// What a `D:activelock` carries.
struct ActiveLock {
    token: String,
    comment: Option<String>,
    timeout: Option<String>,
}

/// Fields of a lock the server reports in headers rather than in the body.
fn lock_from_parts(path: &RelPath, head: &ResponseHead, lock: &ActiveLock) -> Result<RepoLock> {
    let owner = head
        .headers
        .get("X-SVN-Lock-Owner")
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .ok_or_else(|| corrupt("lock response without X-SVN-Lock-Owner"))?
        .to_owned();
    let created = head
        .headers
        .get("X-SVN-Creation-Date")
        .ok_or_else(|| corrupt("lock response without X-SVN-Creation-Date"))
        .and_then(parse_timestamp)?;
    Ok(RepoLock {
        path: RepoDisplayPath::from_rel(path),
        token: lock.token.clone(),
        owner,
        comment: lock.comment.clone().filter(|c| !c.is_empty()),
        created,
        expires: expiry(created, lock.timeout.as_deref()),
    })
}

/// `Second-N` is relative to the creation time; `Infinite` and unknown forms mean no expiry.
fn expiry(created: Timestamp, timeout: Option<&str>) -> Option<Timestamp> {
    let secs: i64 = timeout?.trim().strip_prefix("Second-")?.parse().ok()?;
    created
        .0
        .checked_add(secs.checked_mul(1_000_000)?)
        .map(Timestamp)
}

/// Interprets the response to `LOCK` for `path`.
pub fn parse_lock_response(path: &RelPath, head: &ResponseHead, body: &[u8]) -> Result<RepoLock> {
    if !(200..300).contains(&head.status) {
        return Err(error_from_response(head.status, &head.headers, body));
    }
    let limits = ParserLimits::default();
    let mut xml = XmlReader::new();
    let mut events = xml.feed(body)?;
    events.extend(xml.finish()?);
    // Names of the open elements below the root, to match whole paths.
    let mut stack: Vec<String> = Vec::new();
    let (mut token, mut owner, mut timeout) = (None, None::<String>, None::<String>);
    let mut text = String::new();
    for ev in events {
        match ev {
            XmlEvent::Start(t) => {
                stack.push(t.name.local.clone());
                text.clear();
            }
            XmlEvent::Text(t) => {
                let below = stack.get(2..).map(|s| s.join("/")).unwrap_or_default();
                if below == OWNER_PATH {
                    // The server has already taken the lock: an oversized comment is cut,
                    // never turned into a failure.
                    if text.len() < limits.max_text_bytes {
                        let room = limits.max_text_bytes - text.len();
                        let cut = (0..=room.min(t.len()))
                            .rev()
                            .find(|i| t.is_char_boundary(*i));
                        text.push_str(&t[..cut.unwrap_or(0)]);
                    }
                } else {
                    text.push_str(&t);
                    ParserLimits::check(text.len(), limits.max_href_bytes, "lock field")?;
                }
            }
            XmlEvent::End(_) => {
                // Path below `D:prop/D:lockdiscovery`.
                let below = stack.get(2..).map(|s| s.join("/")).unwrap_or_default();
                match below.as_str() {
                    TOKEN_PATH if token.is_none() => token = Some(validate_lock_token(&text)?),
                    OWNER_PATH => owner = Some(text.clone()),
                    TIMEOUT_PATH => timeout = Some(text.clone()),
                    _ => {}
                }
                stack.pop();
                text.clear();
            }
        }
    }
    let active = ActiveLock {
        token: token.ok_or_else(|| corrupt("LOCK response has no lock token"))?,
        comment: owner,
        timeout,
    };
    lock_from_parts(path, head, &active)
}

/// Interprets the get-lock `PROPFIND` response: `None` when the node is not locked.
pub fn parse_get_lock_response(
    path: &RelPath,
    head: &ResponseHead,
    body: &[u8],
) -> Result<Option<RepoLock>> {
    if !(200..300).contains(&head.status) {
        return Err(error_from_response(head.status, &head.headers, body));
    }
    let entries = parse_chunked(&mut MultiStatusParser::new(), body, 0)?;
    let Some(value) = entries
        .iter()
        .find_map(|e| e.get(&Ns::Dav, "lockdiscovery"))
    else {
        return Ok(None);
    };
    if !value.child_names.iter().any(|c| c == "activelock") {
        return Ok(None);
    }
    let token = value
        .descendant_at(TOKEN_PATH)
        .ok_or_else(|| corrupt("activelock without a lock token"))
        .and_then(validate_lock_token)?;
    let active = ActiveLock {
        token,
        comment: value.descendant_at(OWNER_PATH).map(str::to_owned),
        timeout: value.descendant_at(TIMEOUT_PATH).map(str::to_owned),
    };
    lock_from_parts(path, head, &active).map(Some)
}

//! `LOCK` response and get-lock `PROPFIND` response parsing.

use svn_types::{LockInfo, RelPath, Result, Timestamp};

use crate::errors::error_from_response;
use crate::http::{ResponseHead, parse_chunked};
use crate::propfind::MultiStatusParser;
use crate::util::{corrupt, parse_timestamp};
use crate::xml::{Ns, XmlEvent, XmlReader};

/// What a `D:activelock` carries.
struct ActiveLock {
    token: String,
    comment: Option<String>,
    timeout: Option<String>,
}

/// Fields of a lock the server reports in headers rather than in the body.
fn lock_from_parts(path: &RelPath, head: &ResponseHead, lock: &ActiveLock) -> Result<LockInfo> {
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
    Ok(LockInfo {
        path: format!("/{path}"),
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

fn token_from_href(href: &str) -> Result<String> {
    let t = href.trim();
    if t.is_empty() {
        return Err(corrupt("empty lock token"));
    }
    Ok(t.to_owned())
}

/// Interprets the response to `LOCK` for `path`.
pub fn parse_lock_response(path: &RelPath, head: &ResponseHead, body: &[u8]) -> Result<LockInfo> {
    if !(200..300).contains(&head.status) {
        return Err(error_from_response(head.status, &head.headers, body));
    }
    let mut xml = XmlReader::new();
    let mut events = xml.feed(body)?;
    events.extend(xml.finish()?);
    let (mut in_lock, mut field) = (false, None::<&str>);
    let (mut token, mut owner, mut timeout) = (String::new(), String::new(), String::new());
    for ev in events {
        match ev {
            XmlEvent::Start(t) if t.name.is(&Ns::Dav, "activelock") => in_lock = true,
            XmlEvent::Start(t) if in_lock && t.name.is(&Ns::Dav, "href") => field = Some("href"),
            XmlEvent::Start(t) if in_lock && t.name.is(&Ns::Dav, "owner") => field = Some("owner"),
            XmlEvent::Start(t) if in_lock && t.name.is(&Ns::Dav, "timeout") => {
                field = Some("timeout")
            }
            XmlEvent::Text(t) => match field {
                Some("href") => token.push_str(&t),
                Some("owner") => owner.push_str(&t),
                Some("timeout") => timeout.push_str(&t),
                _ => {}
            },
            XmlEvent::End(_) => field = None,
            _ => {}
        }
    }
    if !in_lock {
        return Err(corrupt("LOCK response has no D:activelock"));
    }
    let active = ActiveLock {
        token: token_from_href(&token)?,
        comment: Some(owner),
        timeout: Some(timeout),
    };
    lock_from_parts(path, head, &active)
}

/// Interprets the get-lock `PROPFIND` response: `None` when the node is not locked.
pub fn parse_get_lock_response(
    path: &RelPath,
    head: &ResponseHead,
    body: &[u8],
) -> Result<Option<LockInfo>> {
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
        .descendants
        .iter()
        .find(|(n, t)| n == "href" && !t.trim().is_empty())
        .map(|(_, t)| token_from_href(t))
        .transpose()?
        .ok_or_else(|| corrupt("activelock without a lock token"))?;
    let active = ActiveLock {
        token,
        comment: value.descendant("owner").map(str::to_owned),
        timeout: value.descendant("timeout").map(str::to_owned),
    };
    lock_from_parts(path, head, &active).map(Some)
}

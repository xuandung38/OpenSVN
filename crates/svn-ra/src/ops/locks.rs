//! Remote operation: locks. `get-locks-report`, `LOCK` and `UNLOCK` on public URLs.
//!
//! Failures keep the server's own Subversion error when it sent one; otherwise the status is
//! read the way libsvn_ra_serf reads it for lock requests (400 = no such lock, 405 = path not
//! in HEAD, 423 = locked by someone else).

use svn_protocol::locks::{
    GetLocksParser, LockOptions, get_lock_request, get_locks_request, lock_request,
    parse_get_lock_response, parse_lock_response, unlock_request,
};
use svn_protocol::{ResponseHead, error_from_response, map_http_status};
use svn_types::{CancelToken, Depth, ErrorCode, LockInfo, RelPath, Result, Revnum, SvnError};

use super::read::stream_items;
use super::resume::ReadRetry;
use super::update::{RaWire, buffered_headers, error_of, headers_of};

/// Classification of a failed `LOCK`/`UNLOCK` that carried no classified Subversion error.
fn lock_status_code(status: u16) -> ErrorCode {
    match status {
        400 => ErrorCode::LockTokenInvalid,
        405 => ErrorCode::OutOfDate,
        423 => ErrorCode::LockedByOther,
        s => map_http_status(s),
    }
}

/// Keeps a classified server error; reclassifies an unclassified one by the HTTP status.
fn lock_error(status: u16, mut e: SvnError) -> SvnError {
    if e.code == ErrorCode::Internal {
        e.code = lock_status_code(status);
    }
    e
}

/// Whether a lock reported for the repository path `lock_path` (`/trunk/a`) is within `depth`
/// of the repository path `scope`. Locks outside the scope are not returned, whatever the
/// server sends.
fn in_scope(scope: &RelPath, depth: Depth, lock_path: &str) -> bool {
    let Ok(p) = RelPath::new(lock_path.strip_prefix('/').unwrap_or(lock_path)) else {
        return false;
    };
    match depth {
        Depth::Empty => p == *scope,
        Depth::Files | Depth::Immediates => p == *scope || p.parent().as_ref() == Some(scope),
        Depth::Infinity | Depth::Exclude | Depth::Unknown => p.starts_with(scope),
    }
}

/// Locks at or below `path` (session-relative) up to `depth`. Locks the server reports outside
/// that scope are dropped.
pub(crate) async fn get_locks(
    wire: &RaWire,
    path: &RelPath,
    depth: Depth,
    cancel: &CancelToken,
) -> Result<Vec<LockInfo>> {
    let repo = wire.repo_path(path)?;
    // Nothing reaches the caller before the whole answer is in: a report that breaks off is
    // asked for again from the start.
    let mut retry = ReadRetry::new(wire, "lock report");
    loop {
        let req = get_locks_request(wire.urls(), &repo, depth)?;
        let attempt = async {
            let resp = wire.send(req, cancel).await?;
            // A path that is not in HEAD answers 404; it has no locks (libsvn_ra_serf does the
            // same).
            if resp.status().as_u16() == 404 {
                return Ok(Vec::new());
            }
            let resp = RaWire::require_success(resp, cancel).await?;
            let mut locks = Vec::new();
            let mut parser = GetLocksParser::new();
            stream_items(resp, &mut parser, cancel, |l: LockInfo| {
                if in_scope(&repo, depth, l.path.as_str()) {
                    locks.push(l);
                } else {
                    tracing::warn!("skipping a lock outside the requested scope");
                }
                Ok(())
            })
            .await?;
            Ok(locks)
        }
        .await;
        match attempt {
            Ok(locks) => return Ok(locks),
            Err(e) => retry.again(e, cancel).await?,
        }
    }
}

/// Locks `path`. `base_rev` makes the server refuse the lock when the file changed since.
pub(crate) async fn lock(
    wire: &RaWire,
    path: &RelPath,
    comment: Option<&str>,
    steal: bool,
    base_rev: Option<Revnum>,
    cancel: &CancelToken,
) -> Result<LockInfo> {
    let repo = wire.repo_path(path)?;
    let opts = LockOptions {
        comment: comment.map(str::to_owned),
        steal,
        current_rev: base_rev,
        timeout_secs: None,
    };
    let req = lock_request(wire.urls(), &repo, &opts)?;
    let resp = wire.send(req, cancel).await?;
    let status = resp.status().as_u16();
    let head = ResponseHead::new(status, headers_of(&resp));
    let body = RaWire::read_small(resp, cancel).await?;
    if !(200..300).contains(&status) {
        return Err(lock_error(
            status,
            error_from_response(status, &head.headers, &body),
        ));
    }
    parse_lock_response(&repo, &head, &body)
}

/// Token of the lock currently on `repo`, `None` when it is not locked.
async fn current_token(
    wire: &RaWire,
    repo: &RelPath,
    cancel: &CancelToken,
) -> Result<Option<String>> {
    let req = get_lock_request(wire.urls(), repo)?;
    let resp = wire.fetch_small(req, cancel).await?;
    let head = ResponseHead::new(resp.status.as_u16(), buffered_headers(&resp));
    let body = resp.body;
    Ok(parse_get_lock_response(repo, &head, &body)?.map(|l| l.token))
}

/// Unlocks `path` with `token`. With `break_lock` and no token the current token is looked up
/// first, like `svn unlock --force`.
pub(crate) async fn unlock(
    wire: &RaWire,
    path: &RelPath,
    token: Option<&str>,
    break_lock: bool,
    cancel: &CancelToken,
) -> Result<()> {
    let repo = wire.repo_path(path)?;
    let token = match token {
        Some(t) => t.to_owned(),
        None if break_lock => current_token(wire, &repo, cancel).await?.ok_or_else(|| {
            SvnError::new(ErrorCode::LockTokenInvalid, format!("{path} is not locked"))
        })?,
        None => {
            return Err(SvnError::new(
                ErrorCode::LockTokenInvalid,
                format!("no lock token given to unlock {path}"),
            ));
        }
    };
    let req = unlock_request(wire.urls(), &repo, &token, break_lock)?;
    let resp = wire.send(req, cancel).await?;
    let status = resp.status().as_u16();
    if resp.status().is_success() {
        return Ok(());
    }
    Err(lock_error(status, error_of(resp, cancel).await))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unclassified_lock_failures_follow_serf() {
        let e = |s| lock_error(s, SvnError::new(ErrorCode::Internal, "x")).code;
        assert_eq!(e(400), ErrorCode::LockTokenInvalid);
        assert_eq!(e(405), ErrorCode::OutOfDate);
        assert_eq!(e(423), ErrorCode::LockedByOther);
        assert_eq!(e(403), ErrorCode::Forbidden);
        let kept = lock_error(423, SvnError::new(ErrorCode::HookFailed, "hook"));
        assert_eq!(kept.code, ErrorCode::HookFailed);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod scope_tests {
    use super::*;

    #[test]
    fn locks_are_filtered_to_the_requested_scope() {
        let trunk = RelPath::new("trunk").unwrap();
        let root = RelPath::root();
        assert!(in_scope(&trunk, Depth::Empty, "/trunk"));
        assert!(!in_scope(&trunk, Depth::Empty, "/trunk/a"));
        assert!(in_scope(&trunk, Depth::Files, "/trunk/a"));
        assert!(in_scope(&trunk, Depth::Immediates, "/trunk/a"));
        assert!(!in_scope(&trunk, Depth::Immediates, "/trunk/d/b"));
        assert!(in_scope(&trunk, Depth::Infinity, "/trunk/d/b"));
        assert!(!in_scope(&trunk, Depth::Infinity, "/trunk2/a"));
        assert!(!in_scope(&trunk, Depth::Infinity, "/branches/a"));
        assert!(in_scope(&root, Depth::Infinity, "/branches/a"));
        assert!(in_scope(&root, Depth::Immediates, "/a"));
        assert!(!in_scope(&root, Depth::Immediates, "/d/a"));
    }
}

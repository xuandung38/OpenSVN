//! Lock responses against recorded traffic.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{DATA_SETS, for_each_set, load, run_split, use_set};
use proptest::prelude::*;
use svn_protocol::locks::{
    GetLocksParser, parse_get_lock_response, parse_get_locks, parse_lock_response,
};
use svn_protocol::{Headers, ResponseHead, parse_timestamp};
use svn_types::{ErrorCode, RelPath, Timestamp};

fn rel(p: &str) -> RelPath {
    RelPath::new(p).unwrap()
}

#[test]
fn lock_response() {
    for_each_set(|| {
        let f = load("locks/lock-ok");
        let lock = parse_lock_response(&rel("trunk/a.txt"), &f.head(), &f.resp_body).unwrap();
        assert_eq!(lock.path.as_str(), "/trunk/a.txt");
        assert_eq!(lock.owner, "alice");
        assert_eq!(lock.comment.as_deref(), Some("locking <it>"));
        assert!(lock.token.starts_with("opaquelocktoken:"));
        assert_eq!(
            format!("<{}>", lock.token),
            f.resp_headers.get("Lock-Token").unwrap()
        );
        assert_eq!(
            lock.created,
            parse_timestamp(f.resp_headers.get("X-SVN-Creation-Date").unwrap()).unwrap()
        );
        assert_eq!(lock.expires, None, "Timeout: Infinite");
    });
}

#[test]
fn stolen_lock_belongs_to_the_new_owner() {
    for_each_set(|| {
        let f = load("locks/lock-steal");
        let lock = parse_lock_response(&rel("trunk/a.txt"), &f.head(), &f.resp_body).unwrap();
        assert_eq!(
            (lock.owner.as_str(), lock.comment.as_deref()),
            ("bob", Some("stolen"))
        );
    });
}

#[test]
fn lock_taken_from_a_working_copy() {
    for_each_set(|| {
        let f = load("locks/lock-from-wc");
        let lock = parse_lock_response(&rel("trunk/a.txt"), &f.head(), &f.resp_body).unwrap();
        assert_eq!(lock.comment.as_deref(), Some("from wc"));
    });
}

#[test]
fn conflicting_lock_is_locked_by_other() {
    for_each_set(|| {
        let f = load("locks/lock-conflict-423");
        assert_eq!(f.status, 423);
        let err = parse_lock_response(&rel("trunk/a.txt"), &f.head(), &f.resp_body).unwrap_err();
        assert_eq!(err.code, ErrorCode::LockedByOther);
        assert_eq!(err.svn_code, Some(160_035));
        assert!(
            err.message.contains("already locked by user 'alice'"),
            "{}",
            err.message
        );
    });
}

#[test]
fn finite_timeout_gives_an_expiry() {
    for_each_set(|| {
        let f = load("locks/lock-ok");
        let body = String::from_utf8(f.resp_body.clone())
            .unwrap()
            .replace("Infinite", "Second-3600");
        let lock = parse_lock_response(&rel("trunk/a.txt"), &f.head(), body.as_bytes()).unwrap();
        assert_eq!(
            lock.expires,
            Some(Timestamp(lock.created.0 + 3_600_000_000))
        );
    });
}

#[test]
fn incomplete_lock_responses_are_corrupt() {
    for_each_set(|| {
        let f = load("locks/lock-ok");
        let mut h = Headers::new();
        for (k, v) in f.resp_headers.iter() {
            if !k.eq_ignore_ascii_case("X-SVN-Lock-Owner") {
                h.push(k, v);
            }
        }
        let err =
            parse_lock_response(&rel("a"), &ResponseHead::new(200, h), &f.resp_body).unwrap_err();
        assert_eq!(err.code, ErrorCode::Corrupt);
        let err =
            parse_lock_response(&rel("a"), &f.head(), b"<D:prop xmlns:D='DAV:'/>").unwrap_err();
        assert_eq!(err.code, ErrorCode::Corrupt);
    });
}

#[test]
fn get_lock_propfind() {
    for_each_set(|| {
        let f = load("propfind/get-lock");
        let lock = parse_get_lock_response(&rel("trunk/a.txt"), &f.head(), &f.resp_body)
            .unwrap()
            .unwrap();
        assert_eq!(lock.path.as_str(), "/trunk/a.txt");
        assert_eq!(lock.owner, "alice");
        assert_eq!(lock.comment.as_deref(), Some("locking <it>"));
        assert!(lock.token.starts_with("opaquelocktoken:"));
    });
}

#[test]
fn get_lock_without_a_comment() {
    for_each_set(|| {
        let f = load("propfind/get-lock-no-comment");
        let lock = parse_get_lock_response(&rel("trunk/src/b.txt"), &f.head(), &f.resp_body)
            .unwrap()
            .unwrap();
        assert_eq!(lock.comment, None);
    });
}

#[test]
fn unlocked_node_has_no_lock() {
    for_each_set(|| {
        let f = load("propfind/get-lock-none");
        assert_eq!(
            parse_get_lock_response(&rel("trunk/src/deep/c.cfg"), &f.head(), &f.resp_body).unwrap(),
            None
        );
    });
}

#[test]
fn get_lock_on_a_missing_node_is_path_not_found() {
    for_each_set(|| {
        let f = load("propfind/stat-missing-404");
        let err = parse_get_lock_response(&rel("nope"), &f.head(), &f.resp_body).unwrap_err();
        assert_eq!(err.code, ErrorCode::PathNotFound);
    });
}

#[test]
fn get_locks_report() {
    for_each_set(|| {
        let f = load("locks/get-locks-report");
        let locks = parse_get_locks(&f.resp_body).unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].path.as_str(), "/trunk/a.txt");
        assert_eq!(locks[0].owner, "alice");
        assert_eq!(locks[0].comment.as_deref(), Some("locking <it>"));
        assert!(locks[0].token.starts_with("opaquelocktoken:"));
        assert_eq!(locks[0].expires, None);
    });
}

#[test]
fn empty_get_locks_report() {
    for_each_set(|| {
        let f = load("locks/get-locks-report-empty");
        assert!(parse_get_locks(&f.resp_body).unwrap().is_empty());
    });
}

#[test]
fn get_locks_rejects_hostile_paths_and_missing_fields() {
    let wrap = |inner: &str| {
        format!(
            r#"<S:get-locks-report xmlns:S="svn:"><S:lock>{inner}</S:lock></S:get-locks-report>"#
        )
    };
    let ok = "<S:path>/a</S:path><S:token>opaquelocktoken:t</S:token><S:owner>o</S:owner><S:creationdate>2026-10-13T08:44:30.014940Z</S:creationdate>";
    assert_eq!(parse_get_locks(wrap(ok).as_bytes()).unwrap().len(), 1);
    for bad in [
        ok.replace("/a", "/a/../../b"),
        ok.replace("/a", "a"),
        ok.replace("<S:token>opaquelocktoken:t</S:token>", ""),
        ok.replace("<S:owner>o</S:owner>", ""),
        ok.replace("2026-10-13T08:44:30.014940Z", "yesterday"),
    ] {
        assert!(parse_get_locks(wrap(&bad).as_bytes()).is_err(), "{bad}");
    }
}

proptest! {
    #[test]
    fn get_locks_chunking(cuts in proptest::collection::vec(0usize..4096, 0..10)) {
        for set in DATA_SETS {
            use_set(set);
            let f = load("locks/get-locks-report");
            let whole = parse_get_locks(&f.resp_body).unwrap();
            prop_assert_eq!(whole, run_split(GetLocksParser::new(), &f.resp_body, &cuts));
        }
    }
}

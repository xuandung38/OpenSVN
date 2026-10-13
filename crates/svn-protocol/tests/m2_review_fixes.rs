//! Virtual transactions, streamed PUT bodies, directory COPY, commit-side token validation and
//! oversized lock comments.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use bytes::Bytes;
use common::{caps, session_url, urls};
use svn_protocol::commit::{
    LockHandling, LockToken, MergeOptions, PutOptions, copy_request, delete_request, merge_request,
    parse_create_txn_response, put_request,
};
use svn_protocol::http::ReadBody;
use svn_protocol::locks::parse_lock_response;
use svn_protocol::{Body, BodyStream, Headers, ResponseHead, TxnName, UrlBuilder};
use svn_types::{ErrorCode, NodeKind, RelPath, Revnum};

fn rel(p: &str) -> RelPath {
    RelPath::new(p).unwrap()
}

#[test]
fn virtual_transactions_use_the_vtxn_stubs() {
    let u = urls();
    let head = ResponseHead::new(201, Headers::from_pairs([("SVN-VTxn-Name", "vt-1")]));
    let txn = parse_create_txn_response(&head, b"").unwrap();
    assert!(txn.virtual_);
    let url = u.txn_root(&txn, &rel("trunk/a")).unwrap();
    assert_eq!(url.path(), "/svn/repo/!svn/vtxr/vt-1/trunk/a");
    assert_eq!(u.txn(&txn).unwrap().path(), "/svn/repo/!svn/vtxn/vt-1");
    let plain = TxnName::plain("9-a");
    assert_eq!(u.txn(&plain).unwrap().path(), "/svn/repo/!svn/txn/9-a");
    // Every builder follows the name's kind.
    let del = delete_request(&u, &txn, &rel("a"), None, None).unwrap();
    assert!(del.url.path().contains("/vtxr/vt-1/"));
    let merge = merge_request(
        &u,
        &rel("trunk"),
        &txn,
        &MergeOptions {
            locks: LockHandling::Release,
            no_merge_response: true,
            lock_tokens: &[],
        },
    )
    .unwrap();
    assert!(String::from_utf8_lossy(merge.body_bytes().unwrap()).contains("/!svn/vtxn/vt-1<"));
    // A plain header wins when both are present.
    let both = ResponseHead::new(
        201,
        Headers::from_pairs([("SVN-Txn-Name", "p-1"), ("SVN-VTxn-Name", "v-1")]),
    );
    assert!(!parse_create_txn_response(&both, b"").unwrap().virtual_);
}

#[test]
fn a_server_with_only_vtxn_stubs_still_works_for_virtual_names() {
    let mut c = caps();
    c.txn_stub = None;
    c.txn_root_stub = None;
    let u = UrlBuilder::new(&session_url(), &c).unwrap();
    assert!(u.txn("p-1").is_err(), "no plain stub announced");
    assert!(u.txn(TxnName::virtual_txn("v-1")).is_ok());
}

#[test]
fn directory_copies_carry_depth_infinity() {
    let u = urls();
    let dir = copy_request(
        &u,
        Revnum(3),
        &rel("trunk/src"),
        NodeKind::Dir,
        "1-a",
        &rel("branches/b"),
    )
    .unwrap();
    assert_eq!(dir.headers.get("Depth"), Some("infinity"));
    let file = copy_request(
        &u,
        Revnum(3),
        &rel("trunk/a.txt"),
        NodeKind::File,
        "1-a",
        &rel("c.txt"),
    )
    .unwrap();
    assert_eq!(file.headers.get("Depth"), None);
}

#[test]
fn commit_side_lock_tokens_get_the_read_side_validation() {
    let u = urls();
    for bad in [
        "anything-goes",
        "mailto:x",
        "opaquelocktoken:a b",
        "opaquelocktoken:a\nb",
        "opaquelocktoken:",
        "",
    ] {
        assert!(
            put_request(
                &u,
                "1-a",
                &rel("f"),
                Body::Empty,
                &PutOptions {
                    lock: Some(LockToken(bad)),
                    ..PutOptions::default()
                }
            )
            .is_err(),
            "PUT {bad:?}"
        );
        assert!(
            delete_request(&u, "1-a", &rel("f"), None, Some(LockToken(bad))).is_err(),
            "DELETE {bad:?}"
        );
        let locks = [(rel("f"), bad.to_owned())];
        let opts = MergeOptions {
            locks: LockHandling::Release,
            no_merge_response: true,
            lock_tokens: &locks,
        };
        assert!(
            merge_request(&u, &rel("trunk"), "1-a", &opts).is_err(),
            "MERGE {bad:?}"
        );
    }
    let ok = put_request(
        &u,
        "1-a",
        &rel("f"),
        Body::Empty,
        &PutOptions {
            lock: Some(LockToken("<opaquelocktoken:ab-1>")),
            ..PutOptions::default()
        },
    )
    .unwrap();
    assert!(
        ok.headers
            .get("If")
            .unwrap()
            .ends_with("(<opaquelocktoken:ab-1>)")
    );
}

#[test]
fn put_bodies_can_be_streamed() {
    let data = vec![7_u8; 200_000];
    let req = put_request(
        &urls(),
        "1-a",
        &rel("big.bin"),
        Body::Stream(Box::new(ReadBody(std::io::Cursor::new(data.clone())))),
        &PutOptions::default(),
    )
    .unwrap();
    let Body::Stream(mut s) = req.body else {
        panic!("expected a stream")
    };
    let (mut total, mut pieces) = (0, 0);
    while let Some(chunk) = s.next_chunk().unwrap() {
        assert!(chunk.len() <= 64 * 1024);
        total += chunk.len();
        pieces += 1;
    }
    assert_eq!(total, data.len());
    assert!(pieces >= 4);
    // Errors from the reader surface as SvnError.
    #[derive(Debug)]
    struct Broken;
    impl std::io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk gone"))
        }
    }
    assert!(ReadBody(Broken).next_chunk().is_err());
    let _ = Bytes::new();
}

fn lock_head() -> ResponseHead {
    ResponseHead::new(
        200,
        Headers::from_pairs([
            ("X-SVN-Lock-Owner", "alice"),
            ("X-SVN-Creation-Date", "2026-10-13T08:44:30.014940Z"),
        ]),
    )
}

#[test]
fn a_huge_lock_comment_never_fails_a_lock_the_server_already_took() {
    let comment = "c".repeat(1 << 20);
    let body = format!(
        r#"<D:prop xmlns:D="DAV:"><D:lockdiscovery><D:activelock><D:owner>{comment}</D:owner><D:timeout>Infinite</D:timeout><D:locktoken><D:href>opaquelocktoken:1234</D:href></D:locktoken></D:activelock></D:lockdiscovery></D:prop>"#
    );
    let lock = parse_lock_response(&rel("a"), &lock_head(), body.as_bytes()).unwrap();
    assert_eq!(lock.token, "opaquelocktoken:1234");
    assert_eq!(
        lock.comment.as_ref().map(String::len),
        Some(1 << 20),
        "within the 16 MiB text cap"
    );

    // Beyond the cap the comment is cut and the token still comes back.
    let huge = "é".repeat(9 << 20);
    let body = format!(
        r#"<D:prop xmlns:D="DAV:"><D:lockdiscovery><D:activelock><D:owner>{huge}</D:owner><D:locktoken><D:href>opaquelocktoken:1234</D:href></D:locktoken></D:activelock></D:lockdiscovery></D:prop>"#
    );
    let lock = parse_lock_response(&rel("a"), &lock_head(), body.as_bytes()).unwrap();
    assert_eq!(lock.token, "opaquelocktoken:1234");
    let c = lock.comment.unwrap();
    assert!(c.len() <= 16 << 20 && c.len() > 15 << 20, "{}", c.len());
    assert!(c.chars().all(|ch| ch == 'é'), "cut on a character boundary");
}

#[test]
fn a_garbled_token_is_still_an_error_even_with_a_huge_comment() {
    let body = r#"<D:prop xmlns:D="DAV:"><D:lockdiscovery><D:activelock><D:locktoken><D:href>junk</D:href></D:locktoken></D:activelock></D:lockdiscovery></D:prop>"#;
    assert_eq!(
        parse_lock_response(&rel("a"), &lock_head(), body.as_bytes())
            .unwrap_err()
            .code,
        ErrorCode::LockTokenInvalid
    );
}

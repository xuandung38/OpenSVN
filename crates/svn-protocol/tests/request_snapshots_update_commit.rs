//! Snapshots of the update, commit and blame builders (including variants the reference client
//! was not observed to send).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use bytes::Bytes;
use common::{caps, urls};
use svn_protocol::HttpRequest;
use svn_protocol::commit::{
    LockHandling, LockToken, MergeOptions, PutOptions, abort_txn_request, copy_request,
    create_txn_request, delete_request, merge_request, mkcol_request, proppatch_request,
    put_request, txn_proppatch_request,
};
use svn_protocol::file_revs::file_revs_request;
use svn_protocol::update::{UpdateMode, UpdateReportBuilder, UpdateRequestOptions};
use svn_types::{Depth, ErrorCode, PropChange, RelPath, Revnum};

fn render(req: &HttpRequest) -> String {
    let mut out = format!("{} {}\n", req.method.as_str(), req.url);
    for (k, v) in req.headers.iter() {
        out.push_str(&format!("{k}: {v}\n"));
    }
    out.push('\n');
    if let Some(b) = req.body_bytes() {
        out.push_str(&String::from_utf8_lossy(b));
        out.push('\n');
    }
    out
}

fn rel(p: &str) -> RelPath {
    RelPath::new(p).unwrap()
}

#[test]
fn update_with_every_option() {
    let o = UpdateRequestOptions {
        mode: UpdateMode::Skelta {
            include_props: true,
        },
        src: rel("trunk"),
        target_revision: Some(Revnum(42)),
        dst: Some(rel("branches/b 1")),
        update_target: Some("sub dir".into()),
        depth: Depth::Immediates,
        ignore_ancestry: true,
        send_copyfrom_args: true,
        text_deltas: false,
    };
    let mut b = UpdateReportBuilder::new(o);
    b.set_path(&RelPath::root(), Revnum(40), Depth::Infinity, true, None);
    b.set_path(
        &rel("a<b>.txt"),
        Revnum(41),
        Depth::Unknown,
        false,
        Some("opaquelocktoken:1"),
    );
    b.link_path(
        &rel("sw"),
        &rel("branches/x"),
        Revnum(40),
        Depth::Files,
        false,
        None,
    );
    b.delete_path(&rel("gone dir"));
    insta::assert_snapshot!(render(&b.build(&urls()).unwrap()));
}

#[test]
fn minimal_head_update() {
    let mut b = UpdateReportBuilder::new(UpdateRequestOptions::new(
        UpdateMode::SendAll,
        RelPath::root(),
    ));
    b.set_path(&RelPath::root(), Revnum(3), Depth::Unknown, false, None);
    insta::assert_snapshot!(render(&b.build(&urls()).unwrap()));
}

#[test]
fn transaction_creation() {
    let u = urls();
    let plain = create_txn_request(&u, &caps(), &[]).unwrap();
    insta::assert_snapshot!("create_txn_plain", render(&plain));
    let msg = "line one\nline \"two\" (3)";
    let with_props = create_txn_request(
        &u,
        &caps(),
        &[("svn:log", msg.as_bytes()), ("svn:author", b"alice")],
    )
    .unwrap();
    insta::assert_snapshot!("create_txn_props", render(&with_props));
    let mut c = caps();
    c.supported_posts = vec!["create-txn".into()];
    let err = create_txn_request(&u, &c, &[("svn:log", b"x")]).unwrap_err();
    assert_eq!(err.code, ErrorCode::ServerUnsupported);
}

#[test]
fn edits() {
    let u = urls();
    let t = "12-ab";
    let lock = Some(LockToken("opaquelocktoken:abc"));
    insta::assert_snapshot!(
        "mkcol",
        render(&mkcol_request(&u, t, &rel("trunk/new dir")).unwrap())
    );
    insta::assert_snapshot!(
        "delete_locked",
        render(&delete_request(&u, t, &rel("trunk/a.txt"), Some(Revnum(7)), lock).unwrap())
    );
    insta::assert_snapshot!(
        "copy",
        render(
            &copy_request(
                &u,
                Revnum(7),
                &rel("trunk/a b"),
                svn_types::NodeKind::File,
                t,
                &rel("branches/c")
            )
            .unwrap()
        )
    );
    let opts = PutOptions {
        base_rev: Some(Revnum(7)),
        base_md5: Some("0123456789abcdef0123456789ABCDEF"),
        result_md5: Some("fedcba9876543210fedcba9876543210"),
        lock,
    };
    insta::assert_snapshot!(
        "put_full",
        render(
            &put_request(
                &u,
                t,
                &rel("trunk/a.txt"),
                svn_protocol::Body::Bytes(Bytes::from_static(b"SVN\0")),
                &opts
            )
            .unwrap()
        )
    );
    assert!(
        put_request(
            &u,
            t,
            &rel("a"),
            svn_protocol::Body::Empty,
            &PutOptions {
                base_md5: Some("zz"),
                ..PutOptions::default()
            }
        )
        .is_err()
    );
}

#[test]
fn property_changes() {
    let u = urls();
    let t = "12-ab";
    let changes = [
        PropChange {
            name: "svn:ignore".into(),
            value: Some(Bytes::from_static(b"*.o\r\nbuild\n")),
        },
        PropChange {
            name: "binary".into(),
            value: Some(Bytes::from_static(&[0, 159, 255, 1])),
        },
        PropChange {
            name: "svn:needs-lock".into(),
            value: Some(Bytes::from_static(b"*")),
        },
        PropChange {
            name: "custom:old".into(),
            value: None,
        },
        PropChange {
            name: "svn:keywords".into(),
            value: None,
        },
    ];
    insta::assert_snapshot!(
        "proppatch_mixed",
        render(&proppatch_request(&u, t, &rel("trunk"), &changes, Some(Revnum(3)), None).unwrap())
    );
    let rev = [PropChange {
        name: "svn:log".into(),
        value: Some(Bytes::from_static(b"message <&>")),
    }];
    insta::assert_snapshot!(
        "proppatch_txn",
        render(&txn_proppatch_request(&u, t, &rev).unwrap())
    );
}

#[test]
fn merge_and_abort() {
    let u = urls();
    let locks = [
        (rel("a.txt"), "opaquelocktoken:1".to_owned()),
        (rel("sub/b & c.txt"), "opaquelocktoken:2".to_owned()),
    ];
    let keep = MergeOptions {
        locks: LockHandling::Keep,
        no_merge_response: false,
        lock_tokens: &locks,
    };
    insta::assert_snapshot!(
        "merge_keep",
        render(&merge_request(&u, &rel("trunk"), "12-ab", &keep).unwrap())
    );
    insta::assert_snapshot!("abort", render(&abort_txn_request(&u, "12-ab").unwrap()));
    assert!(merge_request(&u, &rel("trunk"), "../x", &keep).is_err());
}

#[test]
fn file_revs() {
    let u = urls();
    let req = file_revs_request(
        &u,
        Revnum(9),
        &rel("trunk/a.txt"),
        Revnum(1),
        Revnum(9),
        true,
    )
    .unwrap();
    insta::assert_snapshot!(render(&req));
}

//! Commit editor against a scripted server: request sequence, uploads, locks, failures,
//! cancellation and transaction cleanup.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod ra_write_support;

use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use md5::{Digest, Md5};
use ra_write_support::{
    Bulk, FixedPrompter, ROOT, fixture, requests, server, session_url, transport, wire,
};
use svn_delta::{SvndiffDecoder, WindowApplier};
use svn_ra::ops::commit::{abort_txn, start_commit};
use svn_ra::ops::update::RaWire;
use svn_ra::transport::{
    HttpTransport, MemoryTrustStore, ProxyConfig, RetryPolicy, TransportConfig,
};
use svn_ra_api::{
    CommitEditor, CommitInfo, CommitObserver, NOTHING_COMMITTED, ReplayableSource, TxnAbort,
};
use svn_types::{
    CancelToken, Checksum, ConnectionFailure, ErrorCode, PropChange, RelPath, Result, Revnum,
    SvnError, TxDeltaOp,
};
use tempfile::NamedTempFile;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn md5_hex(data: &[u8]) -> String {
    Md5::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

const TXN: &str = "7-a";

fn p(s: &str) -> RelPath {
    RelPath::new(s).expect("path")
}

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<String>>,
    veto_txn: bool,
    fail_merged: bool,
}

impl Recorder {
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

impl CommitObserver for Recorder {
    fn on_txn_created(&self, txn: &str) -> Result<()> {
        self.events.lock().unwrap().push(format!("txn:{txn}"));
        if self.veto_txn {
            return Err(SvnError::new(ErrorCode::DiskFull, "journal is full"));
        }
        Ok(())
    }

    fn on_merged(&self, info: &CommitInfo) -> Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(format!("merged:{}", info.revision));
        if self.fail_merged {
            return Err(SvnError::new(ErrorCode::DiskFull, "cannot journal"));
        }
        Ok(())
    }
}

fn source(text: &str) -> (NamedTempFile, ReplayableSource, Checksum) {
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(text.as_bytes()).unwrap();
    let md5: [u8; 16] = Md5::digest(text.as_bytes()).into();
    let src = ReplayableSource {
        path: f.path().to_path_buf(),
        translated: true,
        size: Some(text.len() as u64),
    };
    (f, src, Checksum::Md5(md5))
}

async fn mount_txn(srv: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/svn/repo/!svn/me"))
        .respond_with(ResponseTemplate::new(201).append_header("SVN-Txn-Name", TXN))
        .mount(srv)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("{ROOT}/!svn/txn/{TXN}")))
        .respond_with(ResponseTemplate::new(204))
        .mount(srv)
        .await;
}

async fn mount_merge(srv: &MockServer, body: Vec<u8>) {
    Mock::given(method("MERGE"))
        .and(path("/svn/repo/trunk"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(srv)
        .await;
}

fn ok_multistatus() -> ResponseTemplate {
    ResponseTemplate::new(207).set_body_string(
        "<?xml version=\"1.0\"?><D:multistatus xmlns:D=\"DAV:\"><D:response><D:propstat>\
         <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>",
    )
}

async fn begin(
    srv: &MockServer,
    rec: &Arc<Recorder>,
    locks: &[(String, String)],
    cancel: &CancelToken,
) -> Result<Box<dyn CommitEditor>> {
    let w = wire(srv, 4).await;
    start_commit(
        &w,
        "add stuff",
        &[PropChange {
            name: "custom:note".into(),
            value: Some("n".into()),
        }],
        locks,
        false,
        rec.clone(),
        cancel,
    )
    .await
}

/// Text of a full-text svndiff stream.
fn decode_fulltext(svndiff: &[u8]) -> Vec<u8> {
    let mut dec = SvndiffDecoder::new();
    let mut out = Vec::new();
    for w in dec.feed(svndiff).unwrap() {
        for op in &w.ops {
            let TxDeltaOp::NewData { offset, length } = *op else {
                panic!("full text must be NewData only");
            };
            out.extend_from_slice(&w.new_data[offset as usize..(offset + length) as usize]);
        }
    }
    dec.finish().unwrap();
    out
}

#[tokio::test]
async fn full_commit_sequence() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    for (m, status) in [("MKCOL", 201), ("PUT", 201)] {
        Mock::given(method(m))
            .respond_with(ResponseTemplate::new(status))
            .mount(&srv)
            .await;
    }
    Mock::given(method("PROPPATCH"))
        .respond_with(ok_multistatus())
        .mount(&srv)
        .await;
    mount_merge(&srv, fixture("commit/full-merge.resp")).await;

    let rec = Arc::new(Recorder::default());
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    let (_keep, src, md5) = source("hello commit\n");
    let root = ed.open_root(Some(Revnum(5))).await.unwrap();
    let dir = ed.add_directory(&p("dir"), root, None).await.unwrap();
    let file = ed.add_file(&p("dir/f.txt"), dir, None).await.unwrap();
    ed.apply_text(file, None, src, None, md5).await.unwrap();
    ed.change_file_prop(
        file,
        PropChange {
            name: "svn:eol-style".into(),
            value: Some("native".into()),
        },
    )
    .await
    .unwrap();
    ed.close_file(file).await.unwrap();
    ed.close_directory(dir).await.unwrap();
    ed.close_directory(root).await.unwrap();
    let info = ed.close_edit().await.unwrap();

    assert_eq!(info.revision, Revnum(10));
    assert_eq!(info.author.as_deref(), Some("alice"));
    assert_eq!(
        rec.events(),
        [format!("txn:{TXN}"), "merged:r10".to_owned()]
    );

    let reqs = requests(&srv).await;
    let seq: Vec<_> = reqs.iter().map(|(m, p)| (m.as_str(), p.as_str())).collect();
    assert_eq!(
        seq,
        [
            ("POST", "/svn/repo/!svn/me"),
            ("MKCOL", "/svn/repo/!svn/txr/7-a/trunk/dir"),
            ("PUT", "/svn/repo/!svn/txr/7-a/trunk/dir/f.txt"),
            ("PROPPATCH", "/svn/repo/!svn/txr/7-a/trunk/dir/f.txt"),
            ("MERGE", "/svn/repo/trunk"),
        ]
    );
    let all = srv.received_requests().await.unwrap();
    let post = all.iter().find(|r| r.method.as_str() == "POST").unwrap();
    let post_body = String::from_utf8_lossy(&post.body);
    assert!(post_body.contains("svn:log"), "{post_body}");
    assert!(post_body.contains("add stuff"), "{post_body}");
    assert!(post_body.contains("custom:note"), "{post_body}");
    let put = all.iter().find(|r| r.method.as_str() == "PUT").unwrap();
    assert_eq!(decode_fulltext(&put.body), b"hello commit\n");
    assert_eq!(
        put.headers
            .get("X-SVN-Result-Fulltext-MD5")
            .unwrap()
            .to_str()
            .unwrap(),
        md5_hex(b"hello commit\n")
    );
    let merge = all.iter().find(|r| r.method.as_str() == "MERGE").unwrap();
    assert!(
        merge
            .headers
            .get("X-SVN-Options")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("release-locks")
    );
}

#[tokio::test]
async fn modify_delete_and_copy_requests() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    for (m, status) in [("PUT", 204), ("DELETE", 204), ("COPY", 201)] {
        Mock::given(method(m))
            .and(path_prefix_txr())
            .respond_with(ResponseTemplate::new(status))
            .mount(&srv)
            .await;
    }
    Mock::given(method("COPY"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&srv)
        .await;
    mount_merge(&srv, fixture("commit/full-merge.resp")).await;
    let rec = Arc::new(Recorder::default());
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    let (_keep, src, md5) = source("new text");
    let (_keep_base, base, base_md5) = source("old text");
    let root = ed.open_root(Some(Revnum(5))).await.unwrap();
    let f = ed
        .open_file(&p("a.txt"), root, Some(Revnum(4)))
        .await
        .unwrap();
    ed.apply_text(f, Some(base), src, Some(base_md5), md5)
        .await
        .unwrap();
    ed.close_file(f).await.unwrap();
    ed.delete_entry(&p("gone.txt"), Some(Revnum(3)), root)
        .await
        .unwrap();
    let url = format!("{}{ROOT}/trunk/src", srv.uri());
    let d = ed
        .add_directory(&p("copied"), root, Some((&url, Revnum(2))))
        .await
        .unwrap();
    ed.close_directory(d).await.unwrap();
    ed.close_directory(root).await.unwrap();
    ed.close_edit().await.unwrap();

    let all = srv.received_requests().await.unwrap();
    let put = all.iter().find(|r| r.method.as_str() == "PUT").unwrap();
    let hdr =
        |r: &wiremock::Request, n: &str| r.headers.get(n).map(|v| v.to_str().unwrap().to_owned());
    assert_eq!(hdr(put, "X-SVN-Version-Name").as_deref(), Some("4"));
    assert_eq!(
        hdr(put, "X-SVN-Base-Fulltext-MD5"),
        Some(md5_hex(b"old text"))
    );
    let del = all
        .iter()
        .find(|r| r.method.as_str() == "DELETE" && r.url.path().ends_with("gone.txt"))
        .unwrap();
    assert_eq!(hdr(del, "X-SVN-Version-Name").as_deref(), Some("3"));
    let copy = all.iter().find(|r| r.method.as_str() == "COPY").unwrap();
    assert_eq!(copy.url.path(), "/svn/repo/!svn/rvr/2/trunk/src");
    assert!(
        hdr(copy, "Destination")
            .unwrap()
            .ends_with("/svn/repo/!svn/txr/7-a/trunk/copied")
    );
    assert_eq!(hdr(copy, "Depth").as_deref(), Some("infinity"));
}

/// Rebuilds the text an svndiff stream describes on top of `base`.
fn apply_svndiff(base: &[u8], svndiff: &[u8]) -> Vec<u8> {
    let mut dec = SvndiffDecoder::new();
    let mut out = Vec::new();
    {
        let mut ap = WindowApplier::new(std::io::Cursor::new(base.to_vec()), &mut out);
        for w in dec.feed(svndiff).unwrap() {
            ap.apply_window(&w).unwrap();
        }
        dec.finish().unwrap();
        ap.finish().unwrap();
    }
    out
}

/// A large binary-like text and the same text with one byte changed in the middle.
fn big_pair() -> (Vec<u8>, Vec<u8>) {
    let mut x = 0x2545_f491_4f6c_dd1d_u64;
    let base: Vec<u8> = (0..1_500_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let mut target = base.clone();
    target[750_000] ^= 0x5a;
    (base, target)
}

fn file_source(data: &[u8]) -> (NamedTempFile, ReplayableSource, Checksum) {
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(data).unwrap();
    let md5: [u8; 16] = Md5::digest(data).into();
    let src = ReplayableSource {
        path: f.path().to_path_buf(),
        translated: true,
        size: Some(data.len() as u64),
    };
    (f, src, Checksum::Md5(md5))
}

/// Commits a one-byte change of a 1.5 MB file with `local_base` on disk as the pristine
/// (whose recorded MD5 is that of `base`); returns the PUT.
async fn commit_one_byte_change(local_base: &[u8]) -> (wiremock::Request, Vec<u8>, Vec<u8>) {
    let (base, target) = big_pair();
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&srv)
        .await;
    mount_merge(&srv, fixture("commit/full-merge.resp")).await;
    let rec = Arc::new(Recorder::default());
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    let (_keep, src, md5) = file_source(&target);
    let (_keep_base, base_src, _) = file_source(local_base);
    let base_md5 = Checksum::Md5(Md5::digest(&base).into());
    let root = ed.open_root(Some(Revnum(5))).await.unwrap();
    let f = ed
        .open_file(&p("big.bin"), root, Some(Revnum(5)))
        .await
        .unwrap();
    ed.apply_text(f, Some(base_src), src, Some(base_md5), md5)
        .await
        .unwrap();
    ed.close_file(f).await.unwrap();
    ed.close_directory(root).await.unwrap();
    ed.close_edit().await.unwrap();
    let all = srv.received_requests().await.unwrap();
    let put = all
        .into_iter()
        .find(|r| r.method.as_str() == "PUT")
        .unwrap();
    (put, base, target)
}

#[tokio::test]
async fn changed_file_is_sent_as_a_delta_against_its_base() {
    let (base, _) = big_pair();
    let (put, base, target) = commit_one_byte_change(&base).await;
    // svndiff 2 on a low-latency connection, 1 otherwise (as svn picks; both are announced),
    // a few hundred bytes for 1.5 MB.
    assert_eq!(&put.body[..3], b"SVN");
    assert!(matches!(put.body[3], 1 | 2));
    assert!(
        put.body.len() < 2_000,
        "PUT body is {} bytes",
        put.body.len()
    );
    let mut dec = SvndiffDecoder::new();
    let windows = dec.feed(&put.body).unwrap();
    assert!(windows.iter().all(|w| w.sview_len > 0));
    assert!(
        windows
            .iter()
            .flat_map(|w| &w.ops)
            .any(|op| matches!(op, TxDeltaOp::CopyFromSource { .. }))
    );
    assert_eq!(apply_svndiff(&base, &put.body), target);
    let hdr = |n: &str| put.headers.get(n).map(|v| v.to_str().unwrap().to_owned());
    assert_eq!(hdr("X-SVN-Base-Fulltext-MD5"), Some(md5_hex(&base)));
    assert_eq!(hdr("X-SVN-Result-Fulltext-MD5"), Some(md5_hex(&target)));
}

#[tokio::test]
async fn damaged_local_base_falls_back_to_the_full_text() {
    // The pristine on disk is not the text its recorded MD5 names: a delta against it would
    // be applied by the server to the real base and give a wrong file.
    let (mut damaged, _) = big_pair();
    damaged[10] ^= 1;
    let (put, base, target) = commit_one_byte_change(&damaged).await;
    assert_eq!(decode_fulltext(&put.body), target);
    assert_eq!(apply_svndiff(&base, &put.body), target);
    // Truncated or longer pristines are caught the same way.
    let (base, _) = big_pair();
    let (put, _, target) = commit_one_byte_change(&base[..base.len() - 1]).await;
    assert_eq!(decode_fulltext(&put.body), target);
}

fn path_prefix_txr() -> wiremock::matchers::PathRegexMatcher {
    wiremock::matchers::path_regex(r"^/svn/repo/!svn/txr/.*")
}

#[tokio::test]
async fn copy_source_outside_the_repository_is_refused() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    let rec = Arc::new(Recorder::default());
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    let root = ed.open_root(None).await.unwrap();
    let err = ed
        .add_directory(
            &p("x"),
            root,
            Some(("http://evil.test.local/svn/repo/a", Revnum(1))),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::UnsafePath);
    let err = ed
        .add_directory(&p("y"), root, Some(("relative/path", Revnum(1))))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::UnsafePath);
    assert!(requests(&srv).await.iter().all(|(m, _)| m != "COPY"));
}

#[tokio::test]
async fn locks_travel_in_if_header_and_merge_body() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&srv)
        .await;
    mount_merge(&srv, fixture("commit/full-merge.resp")).await;
    let rec = Arc::new(Recorder::default());
    let locks = [("a.txt".to_owned(), "opaquelocktoken:1234-abcd".to_owned())];
    let mut ed = begin(&srv, &rec, &locks, &CancelToken::new())
        .await
        .unwrap();
    let (_keep, src, md5) = source("locked");
    let root = ed.open_root(Some(Revnum(5))).await.unwrap();
    let f = ed
        .open_file(&p("a.txt"), root, Some(Revnum(5)))
        .await
        .unwrap();
    ed.apply_text(f, None, src, None, md5).await.unwrap();
    ed.close_file(f).await.unwrap();
    ed.close_directory(root).await.unwrap();
    ed.close_edit().await.unwrap();
    let all = srv.received_requests().await.unwrap();
    let put = all.iter().find(|r| r.method.as_str() == "PUT").unwrap();
    let if_header = put.headers.get("If").unwrap().to_str().unwrap();
    assert!(
        if_header.contains("opaquelocktoken:1234-abcd"),
        "{if_header}"
    );
    assert!(if_header.contains("/svn/repo/trunk/a.txt"), "{if_header}");
    let merge = all.iter().find(|r| r.method.as_str() == "MERGE").unwrap();
    let body = String::from_utf8_lossy(&merge.body);
    assert!(body.contains("<lock-path>a.txt</lock-path>"), "{body}");
    assert!(body.contains("opaquelocktoken:1234-abcd"), "{body}");
}

#[tokio::test]
async fn deleting_a_directory_presents_the_locks_below_it() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    Mock::given(method("DELETE"))
        .and(path_prefix_txr())
        .respond_with(ResponseTemplate::new(204))
        .mount(&srv)
        .await;
    mount_merge(&srv, fixture("commit/full-merge.resp")).await;
    let rec = Arc::new(Recorder::default());
    let locks = [
        ("d/a.txt".to_owned(), "opaquelocktoken:aaaa".to_owned()),
        ("d/sub/b.txt".to_owned(), "opaquelocktoken:bbbb".to_owned()),
        ("other.txt".to_owned(), "opaquelocktoken:cccc".to_owned()),
    ];
    let mut ed = begin(&srv, &rec, &locks, &CancelToken::new())
        .await
        .unwrap();
    let root = ed.open_root(Some(Revnum(5))).await.unwrap();
    ed.delete_entry(&p("d"), Some(Revnum(5)), root)
        .await
        .unwrap();
    ed.delete_entry(&p("plain"), None, root).await.unwrap();
    ed.close_directory(root).await.unwrap();
    ed.close_edit().await.unwrap();

    let all = srv.received_requests().await.unwrap();
    let del = |suffix: &str| {
        all.iter()
            .find(|r| r.method.as_str() == "DELETE" && r.url.path().ends_with(suffix))
            .unwrap()
    };
    let d = del("/trunk/d");
    let if_header = d.headers.get("If").unwrap().to_str().unwrap();
    assert!(if_header.contains("/svn/repo/trunk/d/a.txt> (<opaquelocktoken:aaaa>)"));
    assert!(if_header.contains("/svn/repo/trunk/d/sub/b.txt> (<opaquelocktoken:bbbb>)"));
    assert!(!if_header.contains("cccc"), "{if_header}");
    assert!(del("/trunk/plain").headers.get("If").is_none());
}

/// Replacing a directory whose old child is locked: mod_dav still sees the lock below the new
/// directory, so `MKCOL` and a directory `COPY` carry the recursive `If:` header (ra_serf's
/// `setup_add_dir_common_headers` / `setup_copy_dir_headers`). Only `DELETE` gets the
/// keep-locks option; whether the locks survive is decided by the `MERGE`.
#[tokio::test]
async fn replacing_a_directory_presents_the_locks_below_it() {
    for keep_locks in [false, true] {
        let srv = server(Bulk::Prefer).await;
        mount_txn(&srv).await;
        for m in ["DELETE", "MKCOL", "PUT"] {
            Mock::given(method(m))
                .and(path_prefix_txr())
                .respond_with(ResponseTemplate::new(201))
                .mount(&srv)
                .await;
        }
        Mock::given(method("COPY"))
            .and(wiremock::matchers::path_regex(r"^/svn/repo/!svn/rvr/.*"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&srv)
            .await;
        mount_merge(&srv, fixture("commit/full-merge.resp")).await;
        let locks = [
            ("rd/f.txt".to_owned(), "opaquelocktoken:ffff".to_owned()),
            ("cd/sub/g.txt".to_owned(), "opaquelocktoken:gggg".to_owned()),
            ("other.txt".to_owned(), "opaquelocktoken:cccc".to_owned()),
        ];
        let w = wire(&srv, 4).await;
        let rec = Arc::new(Recorder::default());
        let mut ed = start_commit(
            &w,
            "replace",
            &[],
            &locks,
            keep_locks,
            rec.clone(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        let root = ed.open_root(Some(Revnum(5))).await.unwrap();
        // Replace `rd` by a new directory with a new `f.txt`.
        ed.delete_entry(&p("rd"), None, root).await.unwrap();
        let rd = ed.add_directory(&p("rd"), root, None).await.unwrap();
        let (_keep, src, md5) = source("y");
        let f = ed.add_file(&p("rd/f.txt"), rd, None).await.unwrap();
        ed.apply_text(f, None, src, None, md5).await.unwrap();
        ed.close_file(f).await.unwrap();
        ed.close_directory(rd).await.unwrap();
        // Replace `cd` by a copy (given as a repository path).
        ed.delete_entry(&p("cd"), None, root).await.unwrap();
        let cd = ed
            .add_directory(&p("cd"), root, Some(("/trunk/src", Revnum(2))))
            .await
            .unwrap();
        ed.close_directory(cd).await.unwrap();
        // A plain new directory with no locks below it.
        let n = ed.add_directory(&p("new"), root, None).await.unwrap();
        ed.close_directory(n).await.unwrap();
        ed.close_directory(root).await.unwrap();
        ed.close_edit().await.unwrap();

        let all = srv.received_requests().await.unwrap();
        let req = |m: &str, suffix: &str| {
            all.iter()
                .find(|r| r.method.as_str() == m && r.url.path().ends_with(suffix))
                .unwrap_or_else(|| panic!("no {m} {suffix}"))
        };
        let hdr = |r: &wiremock::Request, n: &str| {
            r.headers.get(n).map(|v| v.to_str().unwrap().to_owned())
        };
        let mkcol = req("MKCOL", "/trunk/rd");
        assert_eq!(
            hdr(mkcol, "If").as_deref(),
            Some(
                format!(
                    "<{}/svn/repo/trunk/rd/f.txt> (<opaquelocktoken:ffff>)",
                    srv.uri()
                )
                .as_str()
            ),
            "keep_locks={keep_locks}"
        );
        assert!(hdr(mkcol, "X-SVN-Options").is_none());
        let copy = req("COPY", "/svn/repo/!svn/rvr/2/trunk/src");
        assert!(
            hdr(copy, "Destination")
                .unwrap()
                .ends_with("/svn/repo/!svn/txr/7-a/trunk/cd")
        );
        assert_eq!(
            hdr(copy, "If").as_deref(),
            Some(
                format!(
                    "<{}/svn/repo/trunk/cd/sub/g.txt> (<opaquelocktoken:gggg>)",
                    srv.uri()
                )
                .as_str()
            )
        );
        assert!(hdr(copy, "X-SVN-Options").is_none());
        assert!(hdr(req("MKCOL", "/trunk/new"), "If").is_none());
        // The delete of `rd` names the lock too and carries keep-locks when asked.
        let del = req("DELETE", "/trunk/rd");
        assert!(hdr(del, "If").unwrap().contains("opaquelocktoken:ffff"));
        assert_eq!(
            hdr(del, "X-SVN-Options").is_some(),
            keep_locks,
            "keep_locks={keep_locks}"
        );
        // The new file is written with the token, and the MERGE lists it.
        let put = req("PUT", "/trunk/rd/f.txt");
        assert!(hdr(put, "If").unwrap().contains("opaquelocktoken:ffff"));
        let merge = all.iter().find(|r| r.method.as_str() == "MERGE").unwrap();
        let body = String::from_utf8_lossy(&merge.body);
        assert!(body.contains("<lock-path>rd/f.txt</lock-path>"), "{body}");
        assert_eq!(
            hdr(merge, "X-SVN-Options")
                .unwrap_or_default()
                .contains("keep-locks"),
            keep_locks,
            "{:?}",
            hdr(merge, "X-SVN-Options")
        );
    }
}

#[tokio::test]
async fn repository_path_copy_sources_must_be_clean() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    let rec = Arc::new(Recorder::default());
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    let root = ed.open_root(None).await.unwrap();
    for bad in ["/trunk/../etc", "//evil.test.local/x", "/a//b"] {
        let err = ed
            .add_file(&p("x"), root, Some((bad, Revnum(1))))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsafePath, "{bad}");
    }
    assert!(requests(&srv).await.iter().all(|(m, _)| m != "COPY"));
}

/// Dropping an editor without `close_edit`/`abort_edit` deletes the transaction in the
/// background (best effort; the journal and `abort_txn` remain the reliable cleanup).
#[tokio::test]
async fn dropped_editor_deletes_the_transaction_in_the_background() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    let rec = Arc::new(Recorder::default());
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    ed.open_root(None).await.unwrap();
    drop(ed);
    let abort = ("DELETE".to_owned(), format!("{ROOT}/!svn/txn/{TXN}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !requests(&srv).await.contains(&abort) {
        assert!(Instant::now() < deadline, "transaction was not deleted");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn observer_veto_deletes_the_transaction() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    let rec = Arc::new(Recorder {
        veto_txn: true,
        ..Recorder::default()
    });
    let err = begin(&srv, &rec, &[], &CancelToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, ErrorCode::DiskFull);
    let reqs = requests(&srv).await;
    assert_eq!(
        reqs.last().unwrap(),
        &("DELETE".to_owned(), format!("{ROOT}/!svn/txn/{TXN}"))
    );
    assert!(reqs.iter().all(|(m, _)| m != "PUT" && m != "MKCOL"));
}

#[tokio::test]
async fn out_of_date_at_merge_deletes_the_transaction() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    Mock::given(method("MERGE"))
        .respond_with(ResponseTemplate::new(409).set_body_string(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:error xmlns:D=\"DAV:\" \
             xmlns:m=\"http://apache.org/dav/xmlns\" xmlns:C=\"svn:\"><C:error/>\
             <m:human-readable errcode=\"160028\">out of date</m:human-readable></D:error>",
        ))
        .mount(&srv)
        .await;
    let rec = Arc::new(Recorder::default());
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    let root = ed.open_root(None).await.unwrap();
    ed.close_directory(root).await.unwrap();
    let err = ed.close_edit().await.unwrap_err();
    assert_eq!(err.code, ErrorCode::OutOfDate);
    assert_eq!(rec.events(), [format!("txn:{TXN}")]);
    assert_eq!(requests(&srv).await.last().unwrap().0, "DELETE");
}

/// A `D:error` body as mod_dav_svn sends it.
fn dav_error(errcode: u32, message: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:error xmlns:D=\"DAV:\" \
         xmlns:m=\"http://apache.org/dav/xmlns\" xmlns:C=\"svn:\"><C:error/>\
         <m:human-readable errcode=\"{errcode}\">{message}</m:human-readable></D:error>"
    )
}

/// Runs a commit whose `MERGE` gets `answer`; returns the `close_edit` error.
async fn merge_answered(srv: &MockServer, answer: ResponseTemplate) -> SvnError {
    mount_txn(srv).await;
    Mock::given(method("MERGE"))
        .respond_with(answer)
        .mount(srv)
        .await;
    let rec = Arc::new(Recorder::default());
    let mut ed = begin(srv, &rec, &[], &CancelToken::new()).await.unwrap();
    let root = ed.open_root(None).await.unwrap();
    ed.close_directory(root).await.unwrap();
    let err = ed.close_edit().await.unwrap_err();
    assert_eq!(
        rec.events(),
        [format!("txn:{TXN}")],
        "never reported merged"
    );
    err
}

/// Refusals that prove nothing was merged: a 4xx (with or without a `D:error` body, except
/// 408) and a 5xx that carries mod_dav_svn's `D:error` body (for example a failed hook).
#[tokio::test]
async fn definite_merge_refusals_delete_the_transaction() {
    let hook = dav_error(165001, "Commit blocked by pre-commit hook (exit code 1)");
    let cases = [
        (
            409,
            Some(dav_error(160028, "out of date")),
            ErrorCode::OutOfDate,
        ),
        (
            423,
            Some(dav_error(160035, "path is locked")),
            ErrorCode::LockedByOther,
        ),
        (500, Some(hook.clone()), ErrorCode::HookFailed),
        (503, Some(hook), ErrorCode::HookFailed),
        (403, None, ErrorCode::Forbidden),
        (409, None, ErrorCode::OutOfDate),
        (413, None, ErrorCode::Internal),
    ];
    for (status, body, code) in cases {
        let srv = server(Bulk::Prefer).await;
        let mut answer = ResponseTemplate::new(status);
        if let Some(b) = &body {
            answer = answer.set_body_string(b.clone());
        }
        let err = merge_answered(&srv, answer).await;
        assert_eq!(err.code, code, "HTTP {status} {body:?}: {err:?}");
        let reqs = requests(&srv).await;
        assert_eq!(
            reqs.last().unwrap(),
            &("DELETE".to_owned(), format!("{ROOT}/!svn/txn/{TXN}")),
            "HTTP {status}"
        );
    }
}

/// Gateway timeouts and bare 5xx answers do not prove anything: behind a reverse proxy the
/// backend may still be running the commit (a slow pre-commit hook), so the transaction is
/// kept and the error says the outcome is unknown (never retryable).
#[tokio::test]
async fn gateway_and_bare_server_errors_after_merge_keep_the_transaction() {
    let cases = [
        (502, Some("<html>Bad Gateway</html>".to_owned())),
        (504, Some("<html>Gateway Time-out</html>".to_owned())),
        (504, Some(dav_error(160028, "even with a D:error body"))),
        (408, None),
        (502, None),
        (500, None),
        (500, Some("<html>Internal Server Error</html>".to_owned())),
        (503, None),
    ];
    for (status, body) in cases {
        let srv = server(Bulk::Prefer).await;
        let mut answer = ResponseTemplate::new(status);
        if let Some(b) = &body {
            answer = answer.set_body_string(b.clone());
        }
        let err = merge_answered(&srv, answer).await;
        assert_eq!(
            err.code,
            ErrorCode::CommitOutcomeUnknown,
            "HTTP {status} {body:?}"
        );
        assert!(err.message.contains(TXN), "{}", err.message);
        // The server's answer stays visible as the cause.
        let child = err.child.as_ref().expect("cause");
        if body.is_none() {
            assert!(child.message.contains(&status.to_string()), "{child:?}");
        }
        assert_txn_kept(&srv).await;
    }
}

/// Asserts that no `DELETE` was sent, also after giving a stray background task time to run.
async fn assert_txn_kept(srv: &MockServer) {
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reqs = requests(srv).await;
    assert!(reqs.iter().any(|(m, _)| m == "MERGE"), "{reqs:?}");
    assert!(reqs.iter().all(|(m, _)| m != "DELETE"), "{reqs:?}");
}

#[tokio::test]
async fn merge_cancelled_in_flight_keeps_the_transaction() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    Mock::given(method("MERGE"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(fixture("commit/full-merge.resp"))
                .set_delay(Duration::from_millis(800)),
        )
        .mount(&srv)
        .await;
    let rec = Arc::new(Recorder::default());
    let cancel = CancelToken::new();
    let mut ed = begin(&srv, &rec, &[], &cancel).await.unwrap();
    let root = ed.open_root(None).await.unwrap();
    ed.close_directory(root).await.unwrap();
    let canceller = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.cancel();
        })
    };
    let err = ed.close_edit().await.unwrap_err();
    canceller.await.unwrap();
    assert_eq!(err.code, ErrorCode::CommitOutcomeUnknown);
    assert!(err.message.contains(TXN), "{}", err.message);
    assert_eq!(err.child.as_ref().unwrap().code, ErrorCode::Cancelled);
    // Not reported as merged either: the journal still has to reconcile it.
    assert_eq!(rec.events(), [format!("txn:{TXN}")]);
    assert_txn_kept(&srv).await;
}

#[tokio::test]
async fn unreadable_merge_success_keeps_the_transaction() {
    for body in [
        &b"not xml at all"[..],
        b"",
        b"<D:merge-response xmlns:D=\"DAV:\">",
    ] {
        let srv = server(Bulk::Prefer).await;
        mount_txn(&srv).await;
        mount_merge(&srv, body.to_vec()).await;
        let rec = Arc::new(Recorder::default());
        let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
        let root = ed.open_root(None).await.unwrap();
        ed.close_directory(root).await.unwrap();
        let err = ed.close_edit().await.unwrap_err();
        assert_eq!(err.code, ErrorCode::CommitOutcomeUnknown, "{body:?}");
        assert!(err.message.contains(TXN), "{}", err.message);
        assert_txn_kept(&srv).await;
    }
}

#[tokio::test]
async fn post_commit_error_is_a_warning() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    let body = String::from_utf8(fixture("commit/full-merge.resp"))
        .unwrap()
        .replace(
            "</D:updated-set>",
            "</D:updated-set><S:post-commit-err xmlns:S=\"svn:\">hook said no</S:post-commit-err>",
        );
    mount_merge(&srv, body.into_bytes()).await;
    let rec = Arc::new(Recorder::default());
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    let root = ed.open_root(None).await.unwrap();
    ed.close_directory(root).await.unwrap();
    let info = ed.close_edit().await.unwrap();
    assert_eq!(info.revision, Revnum(10));
    assert_eq!(info.post_commit_err.as_deref(), Some("hook said no"));
    assert!(requests(&srv).await.iter().all(|(m, _)| m != "DELETE"));
}

#[tokio::test]
async fn observer_failure_after_merge_names_the_revision() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    mount_merge(&srv, fixture("commit/full-merge.resp")).await;
    let rec = Arc::new(Recorder {
        fail_merged: true,
        ..Recorder::default()
    });
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    let root = ed.open_root(None).await.unwrap();
    ed.close_directory(root).await.unwrap();
    let err = ed.close_edit().await.unwrap_err();
    assert!(err.message.contains("revision r10"), "{}", err.message);
    assert!(requests(&srv).await.iter().all(|(m, _)| m != "DELETE"));
}

/// A wire whose reads give up after 300 ms: a request the server does not answer in time
/// breaks off like a dead connection (`ConnectionFailure::Timeout`).
async fn impatient_wire(srv: &MockServer) -> RaWire {
    let config = TransportConfig {
        proxy: ProxyConfig::Disabled,
        retry: RetryPolicy::none(),
        read_timeout: Duration::from_millis(300),
        ..TransportConfig::default()
    };
    let t = Arc::new(HttpTransport::new(
        config,
        Arc::new(FixedPrompter::default()),
        Arc::new(MemoryTrustStore::new()),
    ));
    RaWire::open(t, session_url(srv), &CancelToken::new())
        .await
        .expect("open wire")
}

/// Adds `a` and closes the edit; the `PUT` never gets its answer in time.
async fn commit_with_a_stalled_put(srv: &MockServer) -> SvnError {
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).set_delay(Duration::from_secs(3)))
        .mount(srv)
        .await;
    let w = impatient_wire(srv).await;
    let rec = Arc::new(Recorder::default());
    let mut ed = start_commit(&w, "m", &[], &[], false, rec, &CancelToken::new())
        .await
        .unwrap();
    let (_keep, src, md5) = source("hello");
    let root = ed.open_root(None).await.unwrap();
    let f = ed.add_file(&p("a"), root, None).await.unwrap();
    ed.apply_text(f, None, src, None, md5).await.unwrap();
    ed.close_file(f).await.unwrap();
    ed.close_directory(root).await.unwrap();
    ed.close_edit().await.unwrap_err()
}

#[tokio::test]
async fn a_broken_upload_whose_transaction_was_deleted_committed_nothing() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    let err = commit_with_a_stalled_put(&srv).await;
    assert_eq!(err.code, ErrorCode::CommitNotApplied, "{err:?}");
    assert_eq!(err.message, NOTHING_COMMITTED);
    // The broken connection is the cause, for the details.
    assert_eq!(err.connection_failure(), Some(ConnectionFailure::Timeout));
    let reqs = requests(&srv).await;
    assert!(reqs.iter().all(|(m, _)| m != "MERGE"), "{reqs:?}");
    assert_eq!(
        reqs.last().unwrap(),
        &("DELETE".to_owned(), format!("{ROOT}/!svn/txn/{TXN}"))
    );
    // The PUT was sent once.
    assert_eq!(reqs.iter().filter(|(m, _)| m == "PUT").count(), 1);
}

#[tokio::test]
async fn a_broken_upload_keeps_its_own_error_when_the_transaction_may_still_exist() {
    for delete in [500, 404] {
        let srv = server(Bulk::Prefer).await;
        Mock::given(method("POST"))
            .and(path("/svn/repo/!svn/me"))
            .respond_with(ResponseTemplate::new(201).append_header("SVN-Txn-Name", TXN))
            .mount(&srv)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(delete))
            .mount(&srv)
            .await;
        let err = commit_with_a_stalled_put(&srv).await;
        // A failed DELETE leaves the transaction unknown; a missing one is not the server
        // confirming it deleted the commit's work. Either way the user sees what broke.
        assert_eq!(err.code, ErrorCode::Timeout, "DELETE {delete}: {err:?}");
        assert_eq!(err.connection_failure(), Some(ConnectionFailure::Timeout));
    }
}

#[tokio::test]
async fn a_refused_upload_is_not_turned_into_nothing_committed() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(409).set_body_string(
            "<?xml version=\"1.0\"?><D:error xmlns:D=\"DAV:\" xmlns:m=\"http://apache.org/dav/xmlns\" \
             xmlns:C=\"svn:\"><C:error/><m:human-readable errcode=\"160024\">out of date\
             </m:human-readable></D:error>",
        ))
        .mount(&srv)
        .await;
    let rec = Arc::new(Recorder::default());
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    let (_keep, src, md5) = source("hello");
    let root = ed.open_root(None).await.unwrap();
    let f = ed.add_file(&p("a"), root, None).await.unwrap();
    ed.apply_text(f, None, src, None, md5).await.unwrap();
    ed.close_file(f).await.unwrap();
    ed.close_directory(root).await.unwrap();
    let err = ed.close_edit().await.unwrap_err();
    assert_ne!(err.code, ErrorCode::CommitNotApplied, "{err:?}");
    assert_eq!(err.svn_code, Some(160_024), "{err:?}");
}

#[tokio::test]
async fn abort_edit_says_whether_the_transaction_was_deleted() {
    for (status, outcome) in [(204, TxnAbort::Deleted), (404, TxnAbort::Gone)] {
        let srv = server(Bulk::Prefer).await;
        Mock::given(method("POST"))
            .and(path("/svn/repo/!svn/me"))
            .respond_with(ResponseTemplate::new(201).append_header("SVN-Txn-Name", TXN))
            .mount(&srv)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&srv)
            .await;
        let rec = Arc::new(Recorder::default());
        let ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
        assert_eq!(ed.abort_edit().await.unwrap(), outcome, "DELETE {status}");
    }
}

#[tokio::test]
async fn abort_edit_deletes_the_transaction() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    Mock::given(method("MKCOL"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&srv)
        .await;
    let rec = Arc::new(Recorder::default());
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    let root = ed.open_root(None).await.unwrap();
    ed.add_directory(&p("d"), root, None).await.unwrap();
    ed.abort_edit().await.unwrap();
    assert_eq!(
        requests(&srv).await.last().unwrap(),
        &("DELETE".to_owned(), format!("{ROOT}/!svn/txn/{TXN}"))
    );
}

#[tokio::test]
async fn open_nodes_at_close_edit_are_an_error_and_clean_up() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    let rec = Arc::new(Recorder::default());
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    ed.open_root(None).await.unwrap();
    let err = ed.close_edit().await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Internal);
    assert_eq!(requests(&srv).await.last().unwrap().0, "DELETE");
}

#[tokio::test]
async fn checksum_mismatch_is_caught_before_upload() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    let rec = Arc::new(Recorder::default());
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    let (_keep, src, _) = source("actual");
    let (_k2, _s2, wrong) = source("promised");
    let root = ed.open_root(None).await.unwrap();
    let f = ed.add_file(&p("a"), root, None).await.unwrap();
    ed.apply_text(f, None, src, None, wrong).await.unwrap();
    // The text is read before close_file returns, so the mismatch is reported there.
    let err = ed.close_file(f).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Corrupt);
    assert!(requests(&srv).await.iter().all(|(m, _)| m != "PUT"));
    ed.abort_edit().await.unwrap();
    assert!(rec.events().iter().all(|e| !e.starts_with("merged")));
}

#[tokio::test]
async fn a_refused_upload_stops_the_commit_before_the_merge() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(423))
        .mount(&srv)
        .await;
    mount_merge(&srv, fixture("commit/full-merge.resp")).await;
    let rec = Arc::new(Recorder::default());
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    let root = ed.open_root(None).await.unwrap();
    let f = ed.add_file(&p("a"), root, None).await.unwrap();
    let (keep, src, md5) = source("text");
    ed.apply_text(f, None, src, None, md5).await.unwrap();
    // The PUT goes on in the background; the source may go once close_file returned.
    ed.close_file(f).await.unwrap();
    drop(keep);
    ed.close_directory(root).await.unwrap();
    let err = ed.close_edit().await.unwrap_err();
    assert_ne!(err.code, ErrorCode::CommitOutcomeUnknown);
    let reqs = requests(&srv).await;
    assert!(reqs.iter().any(|(m, _)| m == "PUT"));
    assert!(reqs.iter().all(|(m, _)| m != "MERGE"));
    assert_eq!(reqs.last().unwrap().0, "DELETE");
}

#[tokio::test]
async fn new_file_without_text_is_created_empty() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&srv)
        .await;
    mount_merge(&srv, fixture("commit/full-merge.resp")).await;
    let rec = Arc::new(Recorder::default());
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    let root = ed.open_root(None).await.unwrap();
    let f = ed.add_file(&p("empty"), root, None).await.unwrap();
    ed.close_file(f).await.unwrap();
    ed.close_directory(root).await.unwrap();
    ed.close_edit().await.unwrap();
    let all = srv.received_requests().await.unwrap();
    let put = all.iter().find(|r| r.method.as_str() == "PUT").unwrap();
    assert_eq!(decode_fulltext(&put.body), b"");
    assert_eq!(&put.body[..3], b"SVN");
}

#[tokio::test]
async fn refused_property_surfaces_from_the_multistatus() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    Mock::given(method("MKCOL"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&srv)
        .await;
    Mock::given(method("PROPPATCH"))
        .respond_with(ResponseTemplate::new(207).set_body_string(
            "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:propstat>\
             <D:status>HTTP/1.1 403 Forbidden</D:status></D:propstat></D:response></D:multistatus>",
        ))
        .mount(&srv)
        .await;
    let rec = Arc::new(Recorder::default());
    let mut ed = begin(&srv, &rec, &[], &CancelToken::new()).await.unwrap();
    let root = ed.open_root(None).await.unwrap();
    let d = ed.add_directory(&p("d"), root, None).await.unwrap();
    ed.change_dir_prop(
        d,
        PropChange {
            name: "svn:ignore".into(),
            value: Some("x".into()),
        },
    )
    .await
    .unwrap();
    let err = ed.close_directory(d).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Forbidden);
}

#[tokio::test]
async fn cancel_stops_uploads_and_abort_cleans_up() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).set_delay(Duration::from_secs(10)))
        .mount(&srv)
        .await;
    let rec = Arc::new(Recorder::default());
    let cancel = CancelToken::new();
    let mut ed = begin(&srv, &rec, &[], &cancel).await.unwrap();
    let (_keep, src, md5) = source("slow");
    let root = ed.open_root(None).await.unwrap();
    let f = ed.add_file(&p("a"), root, None).await.unwrap();
    ed.apply_text(f, None, src, None, md5).await.unwrap();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    // Closing returns at once; the upload goes on in the background until the cancel stops
    // it, and the commit then ends without a MERGE and without its transaction.
    ed.close_file(f).await.unwrap();
    ed.close_directory(root).await.unwrap();
    let err = ed.close_edit().await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(2));
    let reqs = requests(&srv).await;
    assert!(reqs.iter().all(|(m, _)| m != "MERGE"));
    assert_eq!(reqs.last().unwrap().0, "DELETE");
}

#[tokio::test]
async fn cancel_while_a_closed_file_uploads_then_abort_cleans_up() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).set_delay(Duration::from_secs(10)))
        .mount(&srv)
        .await;
    let rec = Arc::new(Recorder::default());
    let cancel = CancelToken::new();
    let mut ed = begin(&srv, &rec, &[], &cancel).await.unwrap();
    let (_keep, src, md5) = source("slow");
    let root = ed.open_root(None).await.unwrap();
    let f = ed.add_file(&p("a"), root, None).await.unwrap();
    ed.apply_text(f, None, src, None, md5).await.unwrap();
    ed.close_file(f).await.unwrap();
    cancel.cancel();
    // Every other call is cancelled, but abort still works and stops the upload.
    assert_eq!(
        ed.open_root(None).await.unwrap_err().code,
        ErrorCode::Cancelled
    );
    let started = Instant::now();
    ed.abort_edit().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(requests(&srv).await.last().unwrap().0, "DELETE");
}

#[tokio::test]
async fn cancelled_close_edit_discards_instead_of_merging() {
    let srv = server(Bulk::Prefer).await;
    mount_txn(&srv).await;
    mount_merge(&srv, fixture("commit/full-merge.resp")).await;
    let rec = Arc::new(Recorder::default());
    let cancel = CancelToken::new();
    let mut ed = begin(&srv, &rec, &[], &cancel).await.unwrap();
    let root = ed.open_root(None).await.unwrap();
    ed.close_directory(root).await.unwrap();
    cancel.cancel();
    let err = ed.close_edit().await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
    let reqs = requests(&srv).await;
    assert!(reqs.iter().all(|(m, _)| m != "MERGE"));
    assert_eq!(reqs.last().unwrap().0, "DELETE");
}

#[tokio::test]
async fn abort_txn_by_name_is_idempotent() {
    let srv = server(Bulk::Prefer).await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&srv)
        .await;
    let w = wire(&srv, 4).await;
    abort_txn(&w, "9-z").await.unwrap();
    abort_txn(&w, "9-z").await.unwrap();
    let reqs = requests(&srv).await;
    assert!(reqs.iter().any(|(_, p)| p == "/svn/repo/!svn/txn/9-z"));
    // A name that could escape the stub is rejected before any request.
    assert!(abort_txn(&w, "../x").await.is_err());
}

#[tokio::test]
async fn server_without_create_txn_with_props_gets_a_proppatch() {
    let srv = MockServer::start().await;
    // Same OPTIONS answer minus the `create-txn-with-props` post.
    Mock::given(method("OPTIONS"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("DAV", "1,2")
                .append_header("SVN-Repository-Root", ROOT)
                .append_header("SVN-Me-Resource", "/svn/repo/!svn/me")
                .append_header("SVN-Rev-Root-Stub", "/svn/repo/!svn/rvr")
                .append_header("SVN-Rev-Stub", "/svn/repo/!svn/rev")
                .append_header("SVN-Txn-Root-Stub", "/svn/repo/!svn/txr")
                .append_header("SVN-Txn-Stub", "/svn/repo/!svn/txn")
                .append_header("SVN-Supported-Posts", "create-txn"),
        )
        .mount(&srv)
        .await;
    mount_txn(&srv).await;
    Mock::given(method("PROPPATCH"))
        .respond_with(ok_multistatus())
        .mount(&srv)
        .await;
    let rec = Arc::new(Recorder::default());
    let w = RaWire::open(
        transport(Arc::new(FixedPrompter::default())),
        session_url(&srv),
        &CancelToken::new(),
    )
    .await
    .unwrap();
    let _ed = start_commit(&w, "msg", &[], &[], false, rec, &CancelToken::new())
        .await
        .unwrap();
    let all = srv.received_requests().await.unwrap();
    let post = all.iter().find(|r| r.method.as_str() == "POST").unwrap();
    assert_eq!(String::from_utf8_lossy(&post.body), "(create-txn)");
    let pp = all
        .iter()
        .find(|r| r.method.as_str() == "PROPPATCH")
        .unwrap();
    assert_eq!(pp.url.path(), "/svn/repo/!svn/txn/7-a");
    assert!(String::from_utf8_lossy(&pp.body).contains("msg"));
}

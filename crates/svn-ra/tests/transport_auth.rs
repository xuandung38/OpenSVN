//! Basic authentication: prompting, single flight, lockout protection, replay and origin scoping.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use common::{
    Handler, Reply, ScriptedPrompter, TestCa, TlsServer, basic, basic_auth_handler,
    transport_trusting,
};
use reqwest::Method;
use svn_ra::transport::{HttpRequest, RequestBody};
use svn_ra_api::{AuthMethod, ReplayableSource};
use svn_types::{CancelToken, ErrorCode};

async fn server(handler: Arc<Handler>) -> (TestCa, TlsServer) {
    let ca = TestCa::new();
    let s = TlsServer::start(&ca.leaf(&["localhost"], false), handler).await;
    (ca, s)
}

fn get(url: url::Url) -> HttpRequest {
    HttpRequest::new(Method::GET, url)
}

#[tokio::test]
async fn prompts_on_401_and_resends_with_basic_credentials() {
    let (ca, srv) = server(basic_auth_handler("VisualSVN Server", "alice", "pw")).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let resp = t
        .send(get(srv.url("/svn/repo")), &CancelToken::new())
        .await
        .expect("authenticated");
    assert_eq!(resp.status().as_u16(), 200);

    let calls = prompter.credential_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    let (origin, attempt, offered) = &calls[0];
    assert_eq!(*attempt, 0);
    assert_eq!(*offered, vec![AuthMethod::Basic]);
    assert_eq!(origin.scheme, "https");
    assert_eq!(origin.host, "localhost");
    assert_eq!(origin.port, srv.addr.port());
    assert_eq!(origin.realm, "VisualSVN Server");

    let seen = srv.requests();
    assert_eq!(seen.len(), 2);
    assert!(seen[0].header("authorization").is_none());
    assert_eq!(seen[1].header("authorization"), Some(basic("alice", "pw")));
}

#[tokio::test]
async fn sends_basic_preemptively_after_success() {
    let (ca, srv) = server(basic_auth_handler("r", "alice", "pw")).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    for path in ["/a", "/b", "/c"] {
        let r = t
            .send(get(srv.url(path)), &CancelToken::new())
            .await
            .expect("ok");
        assert_eq!(r.status().as_u16(), 200);
    }
    assert_eq!(prompter.credential_prompts(), 1);
    let seen = srv.requests();
    // 401 + retry for /a, then one request each for /b and /c with the header already set.
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[2].header("authorization"), Some(basic("alice", "pw")));
    assert_eq!(seen[3].header("authorization"), Some(basic("alice", "pw")));
}

#[tokio::test]
async fn stops_after_three_rejected_credentials() {
    let (ca, srv) = server(basic_auth_handler("r", "alice", "right")).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "wrong");
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let err = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect_err("locked out protection");
    assert_eq!(err.code, ErrorCode::AuthRequired);
    let attempts: Vec<u8> = prompter
        .credential_calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.1)
        .collect();
    assert_eq!(attempts, vec![0, 1, 2]);
    // 1 unauthenticated request + 3 rejected credential submissions.
    assert_eq!(srv.requests().len(), 4);

    // A later request starts a fresh round instead of staying locked out.
    prompter.push_credentials(Some(("alice", "right")));
    let ok = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect("fresh round");
    assert_eq!(ok.status().as_u16(), 200);
    assert_eq!(prompter.credential_prompts(), 4);
    assert_eq!(prompter.credential_calls.lock().unwrap()[3].1, 0);
}

#[tokio::test]
async fn wrong_then_right_credentials_reports_attempt_numbers() {
    let (ca, srv) = server(basic_auth_handler("r", "alice", "right")).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "right");
    prompter.push_credentials(Some(("alice", "wrong")));
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let r = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect("ok");
    assert_eq!(r.status().as_u16(), 200);
    let attempts: Vec<u8> = prompter
        .credential_calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.1)
        .collect();
    assert_eq!(attempts, vec![0, 1]);
}

#[tokio::test]
async fn cancelled_prompt_surfaces_auth_required_without_retrying() {
    let (ca, srv) = server(basic_auth_handler("r", "alice", "pw")).await;
    let prompter = Arc::new(ScriptedPrompter::default());
    prompter.push_credentials(None);
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let err = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::AuthRequired);
    assert_eq!(prompter.credential_prompts(), 1);
    assert_eq!(srv.requests().len(), 1);
}

#[tokio::test]
async fn four_parallel_requests_share_one_prompt() {
    let (ca, srv) = server(basic_auth_handler("r", "alice", "pw")).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    *prompter.delay.lock().unwrap() = Duration::from_millis(200);
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let cancel = CancelToken::new();
    let futs = (0..4).map(|i| {
        let t = Arc::clone(&t);
        let url = srv.url(&format!("/p{i}"));
        let cancel = cancel.clone();
        tokio::spawn(async move { t.send(get(url), &cancel).await.map(|r| r.status().as_u16()) })
    });
    for f in futs {
        assert_eq!(f.await.expect("join").expect("request"), 200);
    }
    assert_eq!(prompter.credential_prompts(), 1);
    assert_eq!(prompter.max_concurrent.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn same_realm_on_another_host_prompts_again_and_never_leaks_credentials() {
    let ca = TestCa::new();
    let leaf = ca.leaf(&["localhost"], false);
    let a = TlsServer::start(&leaf, basic_auth_handler("Shared Realm", "alice", "pw")).await;
    let b = TlsServer::start(&leaf, basic_auth_handler("Shared Realm", "bob", "pw2")).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    prompter.push_credentials(Some(("alice", "pw")));
    prompter.push_credentials(Some(("bob", "pw2")));
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    // Different ports are different origins even on the same host name.
    for srv in [&a, &b] {
        let r = t
            .send(get(srv.url("/")), &CancelToken::new())
            .await
            .expect("ok");
        assert_eq!(r.status().as_u16(), 200);
    }
    assert_eq!(prompter.credential_prompts(), 2);
    let first_b_request = &b.requests()[0];
    assert!(
        first_b_request.header("authorization").is_none(),
        "credentials of another origin must not be sent"
    );
    // Same server reached through a different host name is yet another origin.
    prompter.push_credentials(Some(("alice", "pw")));
    let r = t
        .send(get(a.ip_url("/")), &CancelToken::new())
        .await
        .expect("ok");
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(prompter.credential_prompts(), 3);
    assert!(a.requests().last().is_some());
    assert!(a.requests()[2].header("authorization").is_none());
}

#[tokio::test]
async fn negotiate_only_challenge_does_not_fall_back_to_basic() {
    let handler: Arc<Handler> =
        Arc::new(|_| Reply::status(401).header("www-authenticate", "Negotiate"));
    let (ca, srv) = server(handler).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let err = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect_err("unsupported");
    assert_eq!(err.code, ErrorCode::AuthRequired);
    assert!(err.message.contains("Negotiate"), "{}", err.message);
    assert_eq!(prompter.credential_prompts(), 0);
    assert!(
        srv.requests()
            .iter()
            .all(|r| r.header("authorization").is_none())
    );
}

#[tokio::test]
async fn negotiate_plus_basic_is_offered_to_the_prompter_explicitly() {
    let handler: Arc<Handler> = Arc::new(|seen| {
        if seen.header("authorization").is_some() {
            Reply::ok("in")
        } else {
            Reply::status(401)
                .header("www-authenticate", "Negotiate")
                .header("www-authenticate", "Basic realm=\"AD\"")
        }
    });
    let (ca, srv) = server(handler).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let r = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect("ok");
    assert_eq!(r.status().as_u16(), 200);
    let calls = prompter.credential_calls.lock().unwrap().clone();
    assert_eq!(calls[0].2, vec![AuthMethod::Negotiate, AuthMethod::Basic]);
}

#[tokio::test]
async fn upload_is_replayed_from_the_source_file_after_401() {
    let handler: Arc<Handler> = Arc::new(|seen| {
        if seen.header("authorization").as_deref() == Some(basic("alice", "pw").as_str()) {
            Reply::status(201)
        } else {
            Reply::unauthorized("r")
        }
    });
    let (ca, srv) = server(handler).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let t = transport_trusting(&ca, prompter, |_| {});
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("content.bin");
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::File::create(&path)
        .unwrap()
        .write_all(&payload)
        .unwrap();
    let mut req = HttpRequest::new(Method::PUT, srv.url("/file"));
    req.body = RequestBody::File(ReplayableSource {
        path,
        translated: true,
        size: None,
    });
    let resp = t.send(req, &CancelToken::new()).await.expect("uploaded");
    assert_eq!(resp.status().as_u16(), 201);
    let seen = srv.requests();
    assert_eq!(seen.len(), 2);
    for s in &seen {
        assert_eq!(s.body_len, payload.len() as u64);
        assert_eq!(s.header("content-length"), Some(payload.len().to_string()));
        assert_eq!(&s.body_head[..], &payload[..s.body_head.len()]);
    }
}

#[tokio::test]
async fn one_shot_body_is_not_replayed_after_401() {
    let (ca, srv) = server(basic_auth_handler("r", "alice", "pw")).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let stream =
        futures_util::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from_static(b"data"))]);
    let mut req = HttpRequest::new(Method::PUT, srv.url("/file"));
    req.body = RequestBody::Stream {
        stream: Some(Box::pin(stream)),
        len: Some(4),
    };
    let err = t
        .send(req, &CancelToken::new())
        .await
        .expect_err("cannot replay");
    assert_eq!(err.code, ErrorCode::AuthRequired);
    assert_eq!(prompter.credential_prompts(), 0);
    assert_eq!(srv.requests().len(), 1);
}

#[tokio::test]
async fn authorization_header_set_by_the_caller_is_ignored() {
    let (ca, srv) = server(Arc::new(|_| Reply::ok("x"))).await;
    let t = transport_trusting(&ca, ScriptedPrompter::with_credentials("a", "b"), |_| {});
    let mut req = get(srv.url("/"));
    req.headers
        .insert("authorization", "Basic c3RvbGxlbg==".parse().unwrap());
    t.send(req, &CancelToken::new()).await.expect("ok");
    assert!(srv.requests()[0].header("authorization").is_none());
}

#[tokio::test]
async fn url_userinfo_is_rejected() {
    let (ca, srv) = server(Arc::new(|_| Reply::ok("x"))).await;
    let t = transport_trusting(&ca, ScriptedPrompter::with_credentials("a", "b"), |_| {});
    let mut url = srv.url("/");
    url.set_username("alice").unwrap();
    url.set_password(Some("pw")).unwrap();
    let err = t
        .send(get(url), &CancelToken::new())
        .await
        .expect_err("userinfo");
    assert!(!err.message.contains("pw"));
    assert!(srv.requests().is_empty());
}

#[tokio::test]
async fn cancel_interrupts_a_prompt_that_never_answers() {
    let (ca, srv) = server(basic_auth_handler("r", "alice", "pw")).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    *prompter.delay.lock().unwrap() = Duration::from_secs(3600);
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c2.cancel();
    });
    let start = std::time::Instant::now();
    let err = t
        .send(get(srv.url("/")), &cancel)
        .await
        .expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn cancel_interrupts_a_request_queued_behind_a_prompt() {
    let (ca, srv) = server(basic_auth_handler("r", "alice", "pw")).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    *prompter.delay.lock().unwrap() = Duration::from_secs(3600);
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    // The first request owns the prompt and never gets an answer.
    let owner_cancel = CancelToken::new();
    let owner = {
        let (t, url, c) = (Arc::clone(&t), srv.url("/a"), owner_cancel.clone());
        tokio::spawn(async move { t.send(get(url), &c).await.map(|_| ()) })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let queued_cancel = CancelToken::new();
    let queued = {
        let (t, url, c) = (Arc::clone(&t), srv.url("/b"), queued_cancel.clone());
        tokio::spawn(async move { t.send(get(url), &c).await.map(|_| ()) })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    queued_cancel.cancel();
    let err = tokio::time::timeout(Duration::from_secs(1), queued)
        .await
        .expect("returns within 1s")
        .unwrap()
        .expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert_eq!(prompter.credential_prompts(), 1);
    owner_cancel.cancel();
    let err = owner.await.unwrap().expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
}

#[tokio::test]
async fn server_alternating_realms_cannot_loop_the_client_forever() {
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = Arc::clone(&counter);
    let handler: Arc<Handler> = Arc::new(move |_| {
        let n = c.fetch_add(1, Ordering::SeqCst);
        Reply::unauthorized(if n.is_multiple_of(2) { "A" } else { "B" })
    });
    let (ca, srv) = server(handler).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let err = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect_err("bounded");
    assert_eq!(err.code, ErrorCode::AuthRequired);
    assert!(
        srv.requests().len() <= 10,
        "{} requests",
        srv.requests().len()
    );
    assert!(prompter.credential_prompts() <= 6);
}

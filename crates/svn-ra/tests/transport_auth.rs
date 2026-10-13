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
    assert_eq!(
        *prompter.reasons.lock().unwrap(),
        vec![
            svn_ra_api::PromptReason::Initial,
            svn_ra_api::PromptReason::Rejected
        ]
    );
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
    assert_eq!(err.code, ErrorCode::NegotiateFailed);
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
    // Negotiate is unavailable in this transport, so the prompt says so.
    assert_eq!(
        prompter.reasons.lock().unwrap()[0],
        svn_ra_api::PromptReason::NegotiateFailed
    );
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
    // Wait until the owner really is in the prompt (a fixed sleep raced the TLS handshake on a
    // busy machine, letting the second request become the prompt owner).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while prompter.credential_prompts() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the first request never prompted"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let queued_cancel = CancelToken::new();
    let queued = {
        let (t, url, c) = (Arc::clone(&t), srv.url("/b"), queued_cancel.clone());
        tokio::spawn(async move { t.send(get(url), &c).await.map(|_| ()) })
    };
    // Wait until the server has answered the second request (401): from then on it only waits
    // behind the owner's prompt, so the cancel below hits it while queued.
    while !srv.requests().iter().any(|r| r.path == "/b") {
        assert!(
            std::time::Instant::now() < deadline,
            "the second request never reached the server"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Let it handle the 401 and reach the prompt queue (cancelling slightly earlier still has to
    // end in `Cancelled`, so this is not a correctness wait).
    tokio::task::yield_now().await;
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

#[tokio::test]
async fn unattended_send_never_prompts_and_uses_cached_credentials() {
    let (ca, srv) = server(basic_auth_handler("r", "alice", "pw")).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let t = transport_trusting(&ca, prompter.clone(), |_| {});

    // No cached credentials: the 401 is an error, not a prompt.
    let err = t
        .send_unattended(get(srv.url("/a")), &CancelToken::new())
        .await
        .expect_err("needs credentials");
    assert_eq!(err.code, ErrorCode::AuthRequired);
    assert_eq!(prompter.credential_prompts(), 0);
    assert_eq!(srv.requests().len(), 1);

    // Once an interactive request cached them, unattended requests reuse them silently.
    t.send(get(srv.url("/b")), &CancelToken::new())
        .await
        .expect("interactive");
    assert_eq!(prompter.credential_prompts(), 1);
    let r = t
        .send_unattended(get(srv.url("/c")), &CancelToken::new())
        .await
        .expect("cached credentials");
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(prompter.credential_prompts(), 1);
    let seen = srv.requests();
    assert_eq!(
        seen.last().unwrap().header("authorization"),
        Some(basic("alice", "pw"))
    );
}

#[tokio::test]
async fn stored_credentials_are_used_silently_before_any_prompt() {
    let (ca, srv) = server(basic_auth_handler("r", "alice", "pw")).await;
    let prompter = ScriptedPrompter::with_stored("alice", "pw");
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    for path in ["/a", "/b"] {
        let r = t
            .send(get(srv.url(path)), &CancelToken::new())
            .await
            .expect("stored credentials");
        assert_eq!(r.status().as_u16(), 200);
    }
    assert_eq!(prompter.credential_prompts(), 0);
    assert_eq!(prompter.stored_calls.load(Ordering::SeqCst), 1);
    let seen = srv.requests();
    assert_eq!(seen.len(), 3, "401, retry, then preemptive");
    assert_eq!(seen[1].header("authorization"), Some(basic("alice", "pw")));
}

#[tokio::test]
async fn rejected_stored_credentials_lead_to_a_rejected_prompt() {
    let (ca, srv) = server(basic_auth_handler("r", "alice", "new")).await;
    let prompter = ScriptedPrompter::with_stored("alice", "old");
    *prompter.default_credentials.lock().unwrap() = Some(("alice".into(), "new".into()));
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    t.send(get(srv.url("/a")), &CancelToken::new())
        .await
        .expect("typed credentials");
    assert_eq!(prompter.stored_calls.load(Ordering::SeqCst), 1);
    let calls = prompter.credential_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, 1, "the stored attempt counts");
    assert_eq!(
        prompter.reasons.lock().unwrap().clone(),
        vec![svn_ra_api::PromptReason::Rejected]
    );
    // The app hears about the rejection before the prompt, once.
    let rejected = prompter.stored_rejected.lock().unwrap().clone();
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].reason, svn_ra_api::PromptReason::Rejected);
    assert_eq!(rejected[0].origin, calls[0].0);
    // Typed credentials that work are not stored ones: nothing more is reported.
    t.send(get(srv.url("/b")), &CancelToken::new())
        .await
        .expect("cached typed credentials");
    assert_eq!(prompter.stored_rejections(), 1);
}

#[tokio::test]
async fn unattended_send_uses_stored_credentials_but_never_prompts() {
    let (ca, srv) = server(basic_auth_handler("r", "alice", "pw")).await;
    let prompter = ScriptedPrompter::with_stored("alice", "pw");
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let r = t
        .send_unattended(get(srv.url("/a")), &CancelToken::new())
        .await
        .expect("stored credentials");
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(prompter.credential_prompts(), 0);
    assert_eq!(
        prompter.stored_rejections(),
        0,
        "accepted stored credentials"
    );

    // Wrong stored credentials: AuthRequired, still without a prompt.
    let (ca, srv) = server(basic_auth_handler("r", "alice", "pw")).await;
    let prompter = ScriptedPrompter::with_stored("alice", "stale");
    *prompter.default_credentials.lock().unwrap() = Some(("alice".into(), "pw".into()));
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let err = t
        .send_unattended(get(srv.url("/a")), &CancelToken::new())
        .await
        .expect_err("stale stored credentials");
    assert_eq!(err.code, ErrorCode::AuthRequired);
    assert_eq!(prompter.credential_prompts(), 0);
    assert_eq!(srv.requests().len(), 2);
}

#[tokio::test]
async fn stale_stored_credentials_are_sent_once_per_transport_when_unattended() {
    let (ca, srv) = server(basic_auth_handler("r", "alice", "new")).await;
    let prompter = ScriptedPrompter::with_stored("alice", "stale");
    *prompter.default_credentials.lock().unwrap() = Some(("alice".into(), "new".into()));
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    for i in 0..5 {
        let err = t
            .send_unattended(get(srv.url(&format!("/poll/{i}"))), &CancelToken::new())
            .await
            .expect_err("stale stored credentials");
        assert_eq!(err.code, ErrorCode::AuthRequired);
    }
    let credentialed = srv
        .requests()
        .iter()
        .filter(|r| r.header("authorization").is_some())
        .count();
    assert_eq!(credentialed, 1, "the stale password is submitted once");
    assert_eq!(prompter.stored_calls.load(Ordering::SeqCst), 1);
    assert_eq!(prompter.credential_prompts(), 0);
    // The hook fired once, so the app can stop offering it to the next transport.
    let rejected = prompter.stored_rejected.lock().unwrap().clone();
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].reason, svn_ra_api::PromptReason::Rejected);
    assert_eq!(rejected[0].origin.realm, "r");

    // A second transport (what a poll that does not reuse the transport would do) offers the
    // stored password again; that is why background work must reuse one transport, and why
    // the app must act on the hook.
    let t2 = transport_trusting(&ca, prompter.clone(), |_| {});
    t2.send_unattended(get(srv.url("/poll/again")), &CancelToken::new())
        .await
        .expect_err("still stale");
    assert_eq!(prompter.stored_calls.load(Ordering::SeqCst), 2);
    assert_eq!(prompter.stored_rejections(), 2);
}

#[tokio::test]
async fn stored_attempt_counts_towards_the_three_attempt_cap() {
    let (ca, srv) = server(basic_auth_handler("r", "alice", "right")).await;
    let prompter = ScriptedPrompter::with_stored("alice", "stale");
    prompter.push_credentials(Some(("alice", "wrong1")));
    prompter.push_credentials(Some(("alice", "wrong2")));
    prompter.push_credentials(Some(("alice", "wrong3")));
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let err = t
        .send(get(srv.url("/a")), &CancelToken::new())
        .await
        .expect_err("gives up");
    assert_eq!(err.code, ErrorCode::AuthRequired);
    let sent: Vec<_> = srv
        .requests()
        .iter()
        .filter_map(|r| r.header("authorization"))
        .collect();
    assert_eq!(
        sent,
        vec![
            basic("alice", "stale"),
            basic("alice", "wrong1"),
            basic("alice", "wrong2"),
        ],
        "stored + two prompts = three credentialed requests"
    );
    assert_eq!(prompter.credential_prompts(), 2);
    assert_eq!(
        prompter.stored_rejections(),
        1,
        "only the stored attempt is reported"
    );
    // A later unattended request adds nothing.
    t.send_unattended(get(srv.url("/b")), &CancelToken::new())
        .await
        .expect_err("still refused");
    let credentialed = srv
        .requests()
        .iter()
        .filter(|r| r.header("authorization").is_some())
        .count();
    assert_eq!(credentialed, 3);
    assert_eq!(prompter.stored_calls.load(Ordering::SeqCst), 1);
}

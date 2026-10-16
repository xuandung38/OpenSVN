//! Replays the recorded svn <-> mod_dav_svn sessions in `fixtures/protocol/<session>/` (mod_dav_svn
//! 1.14.3) and `fixtures/protocol-1.15/<session>/` (mod_dav_svn 1.15.0) through the real transport
//! against a scripted HTTPS fake server, and parses every answer with svn-protocol. Proves the captures are replayable and that transport + parsers accept what a
//! real Apache/mod_dav_svn server sends for checkout, update, log and commit.
//!
//! The fake server emulates Basic auth itself (401 exchanges in a transcript are not replayed:
//! unauthenticated requests get the recorded challenge) and serves every other exchange once,
//! matched by method and path in recording order.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use common::{Reply, ScriptedPrompter, TestBody, TestCa, TlsServer};
use reqwest::Method;
use svn_protocol::commit::{parse_create_txn_response, parse_merge_response};
use svn_protocol::log::parse_log;
use svn_protocol::propfind::MultiStatusParser;
use svn_protocol::update::parse_update;
use svn_protocol::{Capabilities, Headers, ResponseHead, UrlBuilder, parse_chunked, xml};
use svn_ra::transport::{HttpRequest, RequestBody};
use svn_types::{CancelToken, RepoUrl};

/// One parsed `.http` message.
#[derive(Clone)]
struct Message {
    start: String,
    headers: Vec<(String, String)>,
    body: Bytes,
}

impl Message {
    fn parse(raw: &[u8]) -> Self {
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("header terminator");
        let head = std::str::from_utf8(&raw[..split]).expect("ascii head");
        let mut lines = head.split("\r\n");
        let start = lines.next().unwrap().to_owned();
        let headers = lines
            .map(|l| {
                let (k, v) = l.split_once(": ").expect("header line");
                (k.to_owned(), v.to_owned())
            })
            .collect();
        Self {
            start,
            headers,
            body: Bytes::copy_from_slice(&raw[split + 4..]),
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone)]
struct Exchange {
    n: usize,
    method: String,
    path: String,
    status: u16,
    req: Message,
    resp: Message,
}

/// Recording sets under `fixtures/`, one per server version, with the same sessions.
const RECORDINGS: [&str; 2] = ["protocol", "protocol-1.15"];

fn protocol_dir(set: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(set)
}

/// Loads `fixtures/<set>/<session>`; `session` is `<set>/<session>` in messages.
fn load(set: &str, name: &str) -> Vec<Exchange> {
    let dir = protocol_dir(set).join(name);
    let session = format!("{set}/{name}");
    let mut out = Vec::new();
    for n in 1.. {
        let req = dir.join(format!("{n:03}-request.http"));
        if !req.exists() {
            break;
        }
        let read = |p: &Path| std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        let req = Message::parse(&read(&req));
        let resp = Message::parse(&read(&dir.join(format!("{n:03}-response.http"))));
        let mut parts = req.start.split(' ');
        let method = parts.next().unwrap().to_owned();
        let path = parts.next().unwrap().to_owned();
        let status = resp.start.split(' ').nth(1).unwrap().parse().unwrap();
        assert_eq!(
            resp.header("Content-Length")
                .map(|v| v.parse::<usize>().unwrap()),
            Some(resp.body.len()),
            "{session} #{n}: Content-Length does not match the body"
        );
        out.push(Exchange {
            n,
            method,
            path,
            status,
            req,
            resp,
        });
    }
    assert!(!out.is_empty(), "no exchanges in {session}");
    out
}

fn reply_for(resp: &Message, status: u16) -> Reply {
    let mut r = Reply::status(status);
    for (k, v) in &resp.headers {
        if k.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let name: &'static str = Box::leak(k.to_ascii_lowercase().into_boxed_str());
        r.headers.push((name, v.clone()));
    }
    r.body = TestBody::Full(Some(resp.body.clone()));
    r
}

/// Fake server state: which exchanges were served.
struct Replay {
    exchanges: Vec<Exchange>,
    served: Mutex<Vec<bool>>,
    challenge: Option<Message>,
}

async fn start(set: &str, session: &str) -> (TlsServer, Arc<Replay>, TestCa) {
    let all = load(set, session);
    let challenge = all.iter().find(|e| e.status == 401).map(|e| e.resp.clone());
    let exchanges: Vec<Exchange> = all.into_iter().filter(|e| e.status != 401).collect();
    let replay = Arc::new(Replay {
        served: Mutex::new(vec![false; exchanges.len()]),
        exchanges,
        challenge,
    });
    let r2 = Arc::clone(&replay);
    let ca = TestCa::new();
    let server = TlsServer::start(
        &ca.leaf(&["localhost"], false),
        Arc::new(move |seen: &common::Seen| {
            let authed = seen.header("authorization").as_deref()
                == Some(common::basic("tester", "tester-pass").as_str());
            if !authed && let Some(c) = &r2.challenge {
                return reply_for(c, 401);
            }
            let mut served = r2.served.lock().unwrap();
            let hit = (0..r2.exchanges.len()).find(|&i| {
                let e = &r2.exchanges[i];
                !served[i] && e.method == seen.method.as_str() && e.path == seen.path
            });
            match hit {
                Some(i) => {
                    served[i] = true;
                    let e = &r2.exchanges[i];
                    assert_eq!(
                        seen.body_len as usize,
                        e.req.body.len(),
                        "#{} request body length",
                        e.n
                    );
                    reply_for(&e.resp, e.status)
                }
                None => Reply::status(500).header("x-replay", "no recorded exchange"),
            }
        }),
    )
    .await;
    (server, replay, ca)
}

fn head_of(m: &Message, status: u16) -> ResponseHead {
    let mut h = Headers::new();
    for (k, v) in &m.headers {
        h.push(k.as_str(), v.as_str());
    }
    ResponseHead::new(status, h)
}

/// Root element local name of an XML request body (`update-report`, `log-report`, ...).
fn root_element(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let start = text.find('<').map(|i| {
        if text[i..].starts_with("<?") {
            text[i..].find("?>").map_or(i, |j| i + j + 2)
        } else {
            i
        }
    })?;
    let tag = text[start..].trim_start().strip_prefix('<')?;
    let name: String = tag
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
        .collect();
    Some(name.rsplit(':').next().unwrap_or(&name).to_owned())
}

/// Replays `fixtures/<set>/<name>`; returns how many exchanges each parser checked.
async fn replay(set: &str, name: &str) -> Vec<(String, usize)> {
    let session = format!("{set}/{name}");
    let (server, replay, ca) = start(set, name).await;
    let prompter = ScriptedPrompter::with_credentials("tester", "tester-pass");
    let transport = common::transport_trusting(&ca, prompter, |_| {});
    let session_url = RepoUrl::parse(server.url("/svn/basic").as_str()).unwrap();
    let cancel = CancelToken::new();
    let mut urls: Option<UrlBuilder> = None;
    let mut checked: Vec<(String, usize)> = Vec::new();
    let mut tick = |what: &str| match checked.iter_mut().find(|(k, _)| k == what) {
        Some((_, n)) => *n += 1,
        None => checked.push((what.to_owned(), 1)),
    };

    for e in replay.exchanges.clone() {
        let mut req = HttpRequest::new(
            Method::from_bytes(e.method.as_bytes()).unwrap(),
            server.url(&e.path),
        );
        for (k, v) in &e.req.headers {
            if matches!(
                k.to_ascii_lowercase().as_str(),
                "host" | "content-length" | "user-agent" | "accept-encoding"
            ) {
                continue;
            }
            req.headers.append(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        if !e.req.body.is_empty() {
            req.body = RequestBody::Bytes(e.req.body.clone());
        }
        let resp = transport
            .send(req, &cancel)
            .await
            .unwrap_or_else(|err| panic!("{session} #{} {} {}: {err:?}", e.n, e.method, e.path));
        let status = resp.status().as_u16();
        assert_eq!(
            status, e.status,
            "{session} #{} {} {}",
            e.n, e.method, e.path
        );
        let body = resp.bytes(64 << 20).await.unwrap();
        assert_eq!(body, e.resp.body, "{session} #{}: body differs", e.n);
        let head = head_of(&e.resp, status);
        let ctx = format!("{session} #{} {} {}", e.n, e.method, e.path);

        match (e.method.as_str(), status) {
            ("OPTIONS", 200) if e.resp.header("SVN-Me-Resource").is_some() => {
                let caps = Capabilities::from_options_response(&session_url, &head, &body)
                    .unwrap_or_else(|err| panic!("{ctx}: {err:?}"));
                urls = Some(UrlBuilder::new(&session_url, &caps).unwrap());
                tick("options");
            }
            ("PROPFIND" | "PROPPATCH", 207) => {
                let mut p = MultiStatusParser::new();
                parse_chunked(&mut p, &body, 0).unwrap_or_else(|err| panic!("{ctx}: {err:?}"));
                tick("multistatus");
            }
            ("REPORT", 200) => match root_element(&e.req.body).as_deref() {
                Some("update-report") => {
                    let u = urls.as_ref().expect("OPTIONS before REPORT");
                    let events =
                        parse_update(u, &body).unwrap_or_else(|err| panic!("{ctx}: {err:?}"));
                    assert!(events.len() > 2, "{ctx}: empty update drive");
                    tick("update-report");
                }
                Some("log-report") => {
                    let items = parse_log(&body).unwrap_or_else(|err| panic!("{ctx}: {err:?}"));
                    assert!(!items.is_empty(), "{ctx}");
                    tick("log-report");
                }
                other => {
                    xml::read_all(&body).unwrap_or_else(|err| panic!("{ctx} {other:?}: {err:?}"));
                    tick("other-report");
                }
            },
            ("POST", 201) => {
                parse_create_txn_response(&head, &body)
                    .unwrap_or_else(|err| panic!("{ctx}: {err:?}"));
                tick("create-txn");
            }
            ("MERGE", 200) => {
                let m = parse_merge_response(&head, &body)
                    .unwrap_or_else(|err| panic!("{ctx}: {err:?}"));
                assert!(m.new_rev.0 > 0, "{ctx}");
                tick("merge");
            }
            (m, _) => tick(&format!("{m} {status}")),
        }
    }
    let served = replay.served.lock().unwrap();
    assert!(
        served.iter().all(|s| *s),
        "{session}: not every exchange was replayed"
    );
    checked
}

fn count(checked: &[(String, usize)], what: &str) -> usize {
    checked
        .iter()
        .find(|(k, _)| k == what)
        .map_or(0, |(_, n)| *n)
}

#[tokio::test]
async fn checkout_send_all_replays_and_parses() {
    for set in RECORDINGS {
        let c = replay(set, "checkout").await;
        assert!(
            count(&c, "options") >= 1 && count(&c, "update-report") >= 1,
            "{set}: {c:?}"
        );
    }
}

#[tokio::test]
async fn checkout_skelta_replays_and_parses() {
    for set in RECORDINGS {
        let c = replay(set, "checkout-skelta").await;
        assert!(
            count(&c, "update-report") >= 1 && count(&c, "GET 200") >= 10,
            "{set}: {c:?}"
        );
    }
}

#[tokio::test]
async fn update_replays_and_parses() {
    for set in RECORDINGS {
        let c = replay(set, "update").await;
        assert!(count(&c, "update-report") >= 1, "{set}: {c:?}");
    }
}

#[tokio::test]
async fn log_replays_and_parses() {
    for set in RECORDINGS {
        let c = replay(set, "log").await;
        assert_eq!(count(&c, "log-report"), 2, "{set}: {c:?}");
    }
}

#[tokio::test]
async fn commit_replays_and_parses() {
    for set in RECORDINGS {
        let c = replay(set, "commit").await;
        assert_eq!(count(&c, "create-txn"), 1, "{set}: {c:?}");
        assert_eq!(count(&c, "merge"), 1, "{set}: {c:?}");
        assert!(
            count(&c, "PUT 201") + count(&c, "PUT 204") >= 2,
            "{set}: {c:?}"
        );
        assert_eq!(count(&c, "DELETE 204"), 1, "{set}: {c:?}");
    }
}

/// The transcripts carry no credentials and only the synthetic host.
#[test]
fn transcripts_are_anonymised() {
    for (set, session) in RECORDINGS.iter().flat_map(|set| {
        ["checkout", "checkout-skelta", "update", "log", "commit"].map(|s| (*set, s))
    }) {
        for e in load(set, session) {
            assert!(
                e.req.header("Authorization").is_none(),
                "{set}/{session} #{}",
                e.n
            );
            assert_eq!(
                e.req.header("Host"),
                Some("svn.test.local"),
                "{set}/{session} #{}",
                e.n
            );
            for m in [&e.req, &e.resp] {
                let text = String::from_utf8_lossy(&m.body);
                assert!(!text.contains("127.0.0.1"), "{set}/{session} #{}", e.n);
                assert!(!text.contains("tester-pass"), "{set}/{session} #{}", e.n);
            }
        }
    }
}

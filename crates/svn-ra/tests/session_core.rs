//! Session core: capabilities come from one cached OPTIONS request.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;

use common::{Reply, ScriptedPrompter, TestCa, TlsServer, basic, transport_trusting};
use svn_ra::session::RaSessionCore;
use svn_types::{CancelToken, RepoUrl};

#[tokio::test]
async fn capabilities_are_fetched_once_after_authenticating() {
    let ca = TestCa::new();
    let srv = TlsServer::start(
        &ca.leaf(&["localhost"], false),
        Arc::new(|seen| {
            if seen.header("authorization").as_deref() == Some(basic("alice", "pw").as_str()) {
                Reply::status(200)
                    .header("dav", "1,2")
                    .header("dav", "http://subversion.tigris.org/xmlns/dav/svn/depth")
                    .header("svn-youngest-rev", "42")
            } else {
                Reply::unauthorized("repo")
            }
        }),
    )
    .await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let session = RaSessionCore::new(
        RepoUrl::parse(srv.url("/svn/repo").as_str()).unwrap(),
        transport_trusting(&ca, prompter.clone(), |_| {}),
    );
    let cancel = CancelToken::new();
    let caps = session.capabilities(&cancel).await.expect("capabilities");
    assert_eq!(
        caps.tokens("dav"),
        vec!["1", "2", "http://subversion.tigris.org/xmlns/dav/svn/depth"]
    );
    assert_eq!(caps.tokens("svn-youngest-rev"), vec!["42"]);
    let before = srv.requests().len();
    session.capabilities(&cancel).await.expect("cached");
    assert_eq!(srv.requests().len(), before);
    assert_eq!(prompter.credential_prompts(), 1);
    assert_eq!(srv.requests()[0].method.as_str(), "OPTIONS");
}

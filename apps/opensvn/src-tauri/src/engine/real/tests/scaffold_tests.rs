//! Wiring scaffold: connection options and the `svn-client` context.

use svn_types::{CancelToken, RepoUrl};

use super::support::{StaticConnector, THIS_BOOT, URL, ctx, fixture, standard_repo};
use crate::engine::OpContext;
use crate::engine::real::{Anchor, ConnectOptions, SessionConnector};

#[tokio::test]
async fn connect_with_defaults_to_a_root_session_whatever_the_options() {
    let connector = StaticConnector(vec![standard_repo()]);
    let url = RepoUrl::parse(&format!("{URL}/trunk")).unwrap();
    for options in [
        ConnectOptions::root(&ctx()),
        ConnectOptions::at_url(&OpContext::background()),
    ] {
        let session = connector
            .connect_with(&url, &options, &CancelToken::new())
            .await
            .unwrap();
        assert_eq!(session.session_url().to_string(), URL);
    }
    assert_eq!(ConnectOptions::at_url(&ctx()).anchor, Anchor::Url);
    assert!(!ConnectOptions::root(&OpContext::background()).interactive);
}

#[test]
fn client_ctx_records_locks_as_this_process() {
    let f = fixture();
    let client = f.engine.client_ctx(&ctx());
    assert_eq!(client.lock_owner.pid, std::process::id());
    assert_eq!(client.lock_owner.boot_id, THIS_BOOT);
    assert!(client.progress.is_none());
}

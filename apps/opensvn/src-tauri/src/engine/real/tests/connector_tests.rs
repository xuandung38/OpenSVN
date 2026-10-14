//! `HttpConnector` with the app's broker and trust store against a scripted HTTPS server:
//! one open per repository, shared credentials, background work without prompts, saved
//! credentials and trusted certificates across a restart, and path-based authorization.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use opensvn_ipc::{AuthPromptKind, AuthResponse, CertDecision as IpcDecision, Settings, WcId};
use secure_store::MemoryStore;
use svn_ra::transport::{ProxyConfig, RetryPolicy, sha256_fingerprint};
use svn_types::{CancelToken, Depth, ErrorCode, RepoUrl, Revnum};
use tokio::sync::watch;

use super::https_svn::{
    PASSWORD, REALM, REPO_UUID, REPO2_UUID, ROOT, ROOT2, Script, SvnServer, USER,
};
use super::support::{fixture_on, standard_repo};
use crate::auth::AuthBroker;
use crate::db::AppDb;
use crate::engine::real::{
    Anchor, ClientRecovery, ConnectOptions, HttpConnector, SessionConnector,
};
use crate::engine::{EngineFacade, OpContext, WcRef};
use crate::ops::events::{Recorded, RecordingEvents};

/// One "app start": broker, events and connector over a database and credential store that
/// outlive it.
struct App {
    events: Arc<RecordingEvents>,
    broker: Arc<AuthBroker>,
    connector: Arc<HttpConnector>,
    settings: watch::Sender<Settings>,
}

fn start(db: &AppDb, store: &Arc<MemoryStore>) -> App {
    start_with_timeout(db, store, Duration::from_secs(20))
}

/// Like [`start`] with a custom sign-in prompt timeout.
fn start_with_timeout(db: &AppDb, store: &Arc<MemoryStore>, timeout: Duration) -> App {
    let events = Arc::new(RecordingEvents::default());
    let broker = Arc::new(AuthBroker::with_timeout(
        events.clone(),
        db.clone(),
        store.clone(),
        timeout,
    ));
    let (settings, rx) = watch::channel(crate::settings::defaults(None));
    let connector = connector_for(&broker, rx);
    App {
        events,
        broker,
        connector,
        settings,
    }
}

/// A connector (and so a fresh transport) over `broker`, without proxy or retry delays.
fn connector_for(
    broker: &Arc<AuthBroker>,
    settings: watch::Receiver<Settings>,
) -> Arc<HttpConnector> {
    Arc::new(HttpConnector::with_config_tweak(
        broker.clone(),
        broker.trust_store(),
        settings,
        broker.invalidations(),
        |c| {
            c.proxy = ProxyConfig::Disabled;
            c.retry = RetryPolicy::none();
        },
    ))
}

impl App {
    fn prompts(&self) -> Vec<opensvn_ipc::AuthPrompt> {
        self.events
            .all()
            .into_iter()
            .filter_map(|r| match r {
                Recorded::Auth(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    /// Answers every prompt in the background: credentials `alice`/`s3cret` (saved when `save`)
    /// and `cert` for certificates.
    fn answer_prompts(&self, save: bool, cert: IpcDecision) -> tokio::task::JoinHandle<()> {
        let (events, broker) = (self.events.clone(), self.broker.clone());
        tokio::spawn(async move {
            let mut answered = 0;
            loop {
                let prompts: Vec<_> = events
                    .all()
                    .into_iter()
                    .filter_map(|r| match r {
                        Recorded::Auth(p) => Some(p),
                        _ => None,
                    })
                    .collect();
                for p in prompts.iter().skip(answered) {
                    let response = match p.kind {
                        AuthPromptKind::Credentials => AuthResponse::Credentials {
                            username: USER.into(),
                            password: PASSWORD.to_owned().into(),
                            save,
                        },
                        AuthPromptKind::Certificate => AuthResponse::Certificate { decision: cert },
                    };
                    // The prompt may still be registering; retry until it is waiting.
                    for _ in 0..200 {
                        if broker.respond(&p.request_id, response.clone()).is_ok() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
                answered = prompts.len();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    }

    /// Waits for prompt number `nth` (from 0), answers it with `response` and returns it.
    async fn respond(&self, nth: usize, response: AuthResponse) -> opensvn_ipc::AuthPrompt {
        for _ in 0..2000 {
            if let Some(p) = self.prompts().get(nth).cloned() {
                for _ in 0..200 {
                    if self.broker.respond(&p.request_id, response.clone()).is_ok() {
                        return p;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                panic!("prompt {nth} never waited for an answer");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no prompt {nth}: {:?}", self.prompts());
    }

    async fn connect(
        &self,
        url: &str,
        anchor: Anchor,
        interactive: bool,
    ) -> Result<Arc<dyn svn_ra_api::RaSession>, svn_types::SvnError> {
        self.connector
            .connect_with(
                &RepoUrl::parse(url).unwrap(),
                &ConnectOptions {
                    anchor,
                    interactive,
                },
                &CancelToken::new(),
            )
            .await
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Trusts the server's self-signed certificate permanently, as an earlier "accept always" did.
fn trust(db: &AppDb, srv: &SvnServer) {
    db.cert_trust(
        "127.0.0.1",
        srv.port,
        &hex(&sha256_fingerprint(&srv.leaf_der)),
        "CN=test",
        "",
    )
    .unwrap();
}

fn store() -> Arc<MemoryStore> {
    Arc::new(MemoryStore::new())
}

#[tokio::test]
async fn one_open_per_repository_and_reparented_sessions_share_credentials() {
    let srv = SvnServer::start(Script {
        require_auth: true,
        ..Script::default()
    })
    .await;
    let (db, store) = (AppDb::open_in_memory().unwrap(), store());
    trust(&db, &srv);
    let app = start(&db, &store);
    let answering = app.answer_prompts(false, IpcDecision::Reject);

    let trunk = app
        .connect(&srv.url("/trunk"), Anchor::Url, true)
        .await
        .unwrap();
    assert_eq!(trunk.session_url().to_string(), srv.url("/trunk"));
    assert_eq!(trunk.repos_root().to_string(), srv.url(""));
    let branch = app
        .connect(&srv.url("/branches/b1"), Anchor::Url, true)
        .await
        .unwrap();
    assert_eq!(branch.session_url().to_string(), srv.url("/branches/b1"));
    let root = app
        .connector
        .connect(
            &RepoUrl::parse(&srv.url("/tags")).unwrap(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(root.session_url().to_string(), srv.url(""));
    let background = app
        .connect(&srv.url("/trunk/src"), Anchor::Url, false)
        .await
        .unwrap();
    // A single OPTIONS opened the repository; every other session was reparented.
    assert_eq!(srv.options_paths(), [format!("{ROOT}/trunk")]);

    // The reparented sessions reuse the credentials: no second prompt, no new challenge.
    let challenges = |s: &SvnServer| {
        s.requests()
            .iter()
            .filter(|r| r.method.as_str() == "UNAUTHORIZED")
            .count()
    };
    let before = challenges(&srv);
    branch
        .ensure_authenticated(&CancelToken::new())
        .await
        .unwrap();
    background
        .ensure_authenticated(&CancelToken::new())
        .await
        .unwrap();
    assert_eq!(challenges(&srv), before);
    assert!(srv.requests().last().unwrap().authorization.is_some());
    assert_eq!(app.prompts().len(), 1);
    answering.abort();
}

#[tokio::test]
async fn background_work_never_prompts() {
    let srv = SvnServer::start(Script {
        require_auth: true,
        ..Script::default()
    })
    .await;
    let (db, store) = (AppDb::open_in_memory().unwrap(), store());
    trust(&db, &srv);
    let app = start(&db, &store);

    // Nothing saved: the background open fails instead of asking.
    let err = app
        .connect(&srv.url("/trunk"), Anchor::Url, false)
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, ErrorCode::AuthRequired);
    // An untrusted certificate fails too, without a certificate prompt.
    let untrusted = SvnServer::start(Script::default()).await;
    let err = app
        .connect(&untrusted.url("/trunk"), Anchor::Root, false)
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, ErrorCode::TlsUntrusted);
    assert!(app.prompts().is_empty(), "{:?}", app.prompts());

    // After an interactive sign-in, background sessions use the cached credentials silently.
    let answering = app.answer_prompts(false, IpcDecision::Reject);
    app.connect(&srv.url("/trunk"), Anchor::Url, true)
        .await
        .unwrap()
        .ensure_authenticated(&CancelToken::new())
        .await
        .unwrap();
    answering.abort();
    let prompts = app.prompts().len();
    let session = app
        .connect(&srv.url("/trunk"), Anchor::Url, false)
        .await
        .unwrap();
    session
        .ensure_authenticated(&CancelToken::new())
        .await
        .unwrap();
    assert_eq!(app.prompts().len(), prompts);
}

#[tokio::test]
async fn saved_credentials_work_silently_after_a_restart() {
    let srv = SvnServer::start(Script {
        require_auth: true,
        ..Script::default()
    })
    .await;
    let (db, store) = (AppDb::open_in_memory().unwrap(), store());
    trust(&db, &srv);
    {
        let first = start(&db, &store);
        let answering = first.answer_prompts(true, IpcDecision::Reject);
        first
            .connect(&srv.url("/trunk"), Anchor::Url, true)
            .await
            .unwrap();
        answering.abort();
        assert_eq!(first.prompts().len(), 1);
    }
    // New broker, transport and connector; same database and credential store.
    let second = start(&db, &store);
    let session = second
        .connect(&srv.url("/trunk"), Anchor::Url, false)
        .await
        .unwrap();
    session
        .ensure_authenticated(&CancelToken::new())
        .await
        .unwrap();
    second
        .connect(&srv.url("/trunk"), Anchor::Url, true)
        .await
        .unwrap()
        .ensure_authenticated(&CancelToken::new())
        .await
        .unwrap();
    assert!(second.prompts().is_empty(), "{:?}", second.prompts());
}

#[tokio::test]
async fn accept_always_survives_a_restart_without_a_prompt() {
    let srv = SvnServer::start(Script::default()).await;
    let (db, store) = (AppDb::open_in_memory().unwrap(), store());
    {
        let first = start(&db, &store);
        let answering = first.answer_prompts(false, IpcDecision::AcceptAlways);
        first
            .connect(&srv.url("/trunk"), Anchor::Url, true)
            .await
            .unwrap();
        answering.abort();
        let prompts = first.prompts();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].kind, AuthPromptKind::Certificate);
    }
    assert!(
        db.cert_is_trusted(
            "127.0.0.1",
            srv.port,
            &hex(&sha256_fingerprint(&srv.leaf_der))
        )
        .unwrap()
    );
    let second = start(&db, &store);
    // Even unattended: the pin was loaded at start-up.
    second
        .connect(&srv.url("/trunk"), Anchor::Url, false)
        .await
        .unwrap()
        .ensure_authenticated(&CancelToken::new())
        .await
        .unwrap();
    assert!(second.prompts().is_empty());
}

#[tokio::test]
async fn sign_out_and_settings_changes_drop_cached_sessions() {
    let srv = SvnServer::start(Script::default()).await;
    let (db, store) = (AppDb::open_in_memory().unwrap(), store());
    trust(&db, &srv);
    let app = start(&db, &store);
    app.connect(&srv.url("/trunk"), Anchor::Url, true)
        .await
        .unwrap();
    app.connect(&srv.url("/trunk"), Anchor::Url, true)
        .await
        .unwrap();
    assert_eq!(srv.options_paths().len(), 1, "cached");
    app.broker.sign_out("some realm").unwrap();
    app.connect(&srv.url("/trunk"), Anchor::Url, true)
        .await
        .unwrap();
    assert_eq!(srv.options_paths().len(), 2, "reopened after sign-out");
    // A change of an unrelated setting keeps the cache; a network setting drops it.
    app.settings
        .send_modify(|s| s.diff_ignore_whitespace = true);
    app.connect(&srv.url("/trunk"), Anchor::Url, true)
        .await
        .unwrap();
    assert_eq!(srv.options_paths().len(), 2);
    app.settings.send_modify(|s| s.network_negotiate = false);
    app.connect(&srv.url("/trunk"), Anchor::Url, true)
        .await
        .unwrap();
    assert_eq!(
        srv.options_paths().len(),
        3,
        "reopened after a network change"
    );
}

#[tokio::test]
async fn trunk_only_authorization_allows_checkout_and_update_of_trunk() {
    let srv = SvnServer::start(Script {
        trunk_only: true,
        ..Script::default()
    })
    .await;
    let (db, store) = (AppDb::open_in_memory().unwrap(), store());
    trust(&db, &srv);
    let app = start(&db, &store);
    let f = fixture_on(
        standard_repo(),
        app.connector.clone(),
        Arc::new(ClientRecovery),
    );
    let ctx = OpContext::detached();
    let client = f.engine.client_ctx(&ctx);
    let tmp = tempfile::tempdir().unwrap();
    let wc = tmp.path().join("wc");

    let session = app
        .connect(&srv.url("/trunk"), Anchor::Url, true)
        .await
        .unwrap();
    let summary = svn_client::checkout(
        &client,
        session.as_ref(),
        &wc,
        Some(Revnum(5)),
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    let _ = summary;
    assert_eq!(
        std::fs::read_to_string(wc.join("a.txt")).unwrap(),
        "hello\nworld\nmore\n"
    );

    // The engine authenticates at the working copy URL, not at the forbidden root.
    let wc_ref = WcRef {
        id: WcId("wc".into()),
        root: std::fs::canonicalize(&wc).unwrap(),
    };
    f.engine.ensure_authenticated(&wc_ref, &ctx).await.unwrap();

    // Background update of trunk through the cached repository session.
    let session = app
        .connect(&srv.url("/trunk"), Anchor::Url, false)
        .await
        .unwrap();
    svn_client::update(
        &client,
        session.as_ref(),
        &wc,
        Some(Revnum(5)),
        Depth::Infinity,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    assert!(Path::new(&wc.join("src/b.txt")).exists());
    assert!(
        !srv.options_paths().iter().any(|p| p == ROOT),
        "the forbidden root was never asked: {:?}",
        srv.options_paths()
    );
    assert!(app.prompts().is_empty());
}

#[tokio::test]
async fn a_rejected_saved_password_is_not_submitted_again_by_a_new_transport() {
    use secure_store::CredentialStore;
    let srv = SvnServer::start(Script {
        require_auth: true,
        ..Script::default()
    })
    .await;
    let (db, store) = (AppDb::open_in_memory().unwrap(), store());
    trust(&db, &srv);
    // A password saved earlier that the server no longer accepts.
    db.account_upsert(&crate::db::AccountRecord {
        scheme: "https",
        host: "127.0.0.1",
        port: srv.port,
        realm: REALM,
        username: USER,
        negotiate: false,
    })
    .unwrap();
    let origin = svn_types::OriginKey {
        scheme: "https".into(),
        host: "127.0.0.1".into(),
        port: srv.port,
        realm: REALM.into(),
    };
    store
        .set(
            &origin,
            USER,
            &secrecy::SecretString::from("old".to_owned()),
        )
        .unwrap();
    let submissions = |s: &SvnServer| {
        s.requests()
            .iter()
            .filter(|r| r.method.as_str() == "UNAUTHORIZED" && r.authorization.is_some())
            .count()
    };
    let app = start(&db, &store);
    let err = app
        .connect(&srv.url("/trunk"), Anchor::Url, false)
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, ErrorCode::AuthRequired);
    assert_eq!(submissions(&srv), 1, "the saved password was tried once");

    // A rebuilt transport (new connector, same broker) does not submit it again.
    let (_tx, rx) = watch::channel(crate::settings::defaults(None));
    let rebuilt = connector_for(&app.broker, rx);
    let err = rebuilt
        .connect_with(
            &RepoUrl::parse(&srv.url("/trunk")).unwrap(),
            &ConnectOptions {
                anchor: Anchor::Url,
                interactive: false,
            },
            &CancelToken::new(),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, ErrorCode::AuthRequired);
    assert_eq!(submissions(&srv), 1);
    assert!(app.prompts().is_empty());

    // Once the user saves the right password it is used silently again.
    let answering = app.answer_prompts(true, IpcDecision::Reject);
    app.connect(&srv.url("/trunk"), Anchor::Url, true)
        .await
        .unwrap();
    answering.abort();
    let (_tx, rx) = watch::channel(crate::settings::defaults(None));
    let after = connector_for(&app.broker, rx);
    after
        .connect_with(
            &RepoUrl::parse(&srv.url("/trunk")).unwrap(),
            &ConnectOptions {
                anchor: Anchor::Url,
                interactive: false,
            },
            &CancelToken::new(),
        )
        .await
        .unwrap();
}

fn credentials(password: &str, save: bool) -> AuthResponse {
    AuthResponse::Credentials {
        username: USER.into(),
        password: password.to_owned().into(),
        save,
    }
}

fn test_origin(srv: &SvnServer) -> svn_types::OriginKey {
    svn_types::OriginKey {
        scheme: "https".into(),
        host: "127.0.0.1".into(),
        port: srv.port,
        realm: REALM.into(),
    }
}

/// Requests that carried credentials and got a 401.
fn rejected_submissions(srv: &SvnServer) -> usize {
    srv.requests()
        .iter()
        .filter(|r| r.method.as_str() == "UNAUTHORIZED" && r.authorization.is_some())
        .count()
}

#[tokio::test]
async fn sibling_repositories_sharing_a_prefix_get_their_own_sessions() {
    let srv = SvnServer::start(Script {
        second_repo: true,
        ..Script::default()
    })
    .await;
    let (db, store) = (AppDb::open_in_memory().unwrap(), store());
    trust(&db, &srv);
    let app = start(&db, &store);
    let repo = app
        .connect(&srv.raw_url(&format!("{ROOT}/trunk")), Anchor::Url, true)
        .await
        .unwrap();
    assert_eq!(repo.repos_root().to_string(), srv.raw_url(ROOT));
    // `/svn/repo` is a string prefix of `/svn/repo2`: the cached session must not serve it.
    let repo2 = app
        .connect(&srv.raw_url(&format!("{ROOT2}/trunk")), Anchor::Url, true)
        .await
        .unwrap();
    assert_eq!(repo2.repos_root().to_string(), srv.raw_url(ROOT2));
    assert_eq!(
        repo2.session_url().to_string(),
        srv.raw_url(&format!("{ROOT2}/trunk"))
    );
    assert_eq!(repo.uuid(), REPO_UUID);
    assert_eq!(repo2.uuid(), REPO2_UUID);
    // Each is cached under its own root afterwards.
    app.connect(
        &srv.raw_url(&format!("{ROOT2}/branches")),
        Anchor::Url,
        true,
    )
    .await
    .unwrap();
    app.connect(&srv.raw_url(&format!("{ROOT}/branches")), Anchor::Url, true)
        .await
        .unwrap();
    assert_eq!(
        srv.options_paths(),
        [format!("{ROOT}/trunk"), format!("{ROOT2}/trunk")]
    );
}

#[tokio::test]
async fn visualsvn_case_differing_urls_reuse_the_cached_repository() {
    let srv = SvnServer::start(Script {
        ignore_case: true,
        ..Script::default()
    })
    .await;
    let (db, store) = (AppDb::open_in_memory().unwrap(), store());
    trust(&db, &srv);
    let app = start(&db, &store);
    // The working copy spells the repository differently from the server's root.
    let trunk = app
        .connect(&srv.raw_url("/svn/REPO/trunk"), Anchor::Url, true)
        .await
        .unwrap();
    assert_eq!(trunk.repos_root().to_string(), srv.raw_url(ROOT));
    for path in ["/svn/REPO/branches/b1", "/svn/Repo/tags", "/svn/repo/trunk"] {
        app.connect(&srv.raw_url(path), Anchor::Url, true)
            .await
            .unwrap();
    }
    assert_eq!(
        srv.options_paths(),
        ["/svn/REPO/trunk"],
        "one OPTIONS for every spelling"
    );
}

#[tokio::test]
async fn case_differing_urls_are_not_folded_for_a_server_that_never_folded() {
    let srv = SvnServer::start(Script {
        ignore_case: true,
        ..Script::default()
    })
    .await;
    let (db, store) = (AppDb::open_in_memory().unwrap(), store());
    trust(&db, &srv);
    let app = start(&db, &store);
    // Opened with the server's own spelling: nothing shows the server ignores case yet, and on
    // a case-sensitive server `/svn/REPO` could be another repository.
    app.connect(&srv.url("/trunk"), Anchor::Url, true)
        .await
        .unwrap();
    app.connect(&srv.raw_url("/svn/REPO/trunk"), Anchor::Url, true)
        .await
        .unwrap();
    // That open showed the folding; from now on other spellings hit the cache.
    app.connect(&srv.raw_url("/svn/Repo/tags"), Anchor::Url, true)
        .await
        .unwrap();
    assert_eq!(
        srv.options_paths(),
        [format!("{ROOT}/trunk"), "/svn/REPO/trunk".to_owned()]
    );
}

#[tokio::test]
async fn a_waiting_prompt_does_not_block_background_work_on_another_repository() {
    let srv = SvnServer::start(Script {
        require_auth: true,
        second_repo: true,
        ..Script::default()
    })
    .await;
    let (db, store) = (AppDb::open_in_memory().unwrap(), store());
    trust(&db, &srv);
    let app = start(&db, &store);
    let connector = app.connector.clone();
    let url = RepoUrl::parse(&srv.raw_url(&format!("{ROOT}/trunk"))).unwrap();
    let interactive = tokio::spawn(async move {
        connector
            .connect_with(
                &url,
                &ConnectOptions {
                    anchor: Anchor::Url,
                    interactive: true,
                },
                &CancelToken::new(),
            )
            .await
            .map(|_| ())
    });
    // The sign-in prompt is up and nobody answers it yet.
    for _ in 0..2000 {
        if !app.prompts().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(app.prompts().len(), 1);
    let background = tokio::time::timeout(
        Duration::from_secs(5),
        app.connect(&srv.raw_url(&format!("{ROOT2}/trunk")), Anchor::Url, false),
    )
    .await
    .expect("background work does not wait for the prompt");
    assert_eq!(background.err().unwrap().code, ErrorCode::AuthRequired);
    // Same repository, other path: does not wait either.
    let same_repo = tokio::time::timeout(
        Duration::from_secs(5),
        app.connect(
            &srv.raw_url(&format!("{ROOT}/branches")),
            Anchor::Url,
            false,
        ),
    )
    .await
    .expect("does not wait for the prompt");
    assert_eq!(same_repo.err().unwrap().code, ErrorCode::AuthRequired);
    assert_eq!(app.prompts().len(), 1, "background work never prompts");

    app.respond(0, credentials(PASSWORD, false)).await;
    interactive.await.unwrap().unwrap();
    // Afterwards background work on the other repository uses the credentials silently.
    app.connect(&srv.raw_url(&format!("{ROOT2}/trunk")), Anchor::Url, false)
        .await
        .unwrap()
        .ensure_authenticated(&CancelToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_typed_password_is_saved_only_after_the_server_accepted_it() {
    use secure_store::CredentialStore;
    let srv = SvnServer::start(Script {
        require_auth: true,
        ..Script::default()
    })
    .await;
    let (db, store) = (AppDb::open_in_memory().unwrap(), store());
    trust(&db, &srv);
    let app = start(&db, &store);
    let origin = test_origin(&srv);

    // A mistyped password with "save", then a cancel: nothing reaches the store.
    let connecting = {
        let connector = app.connector.clone();
        let url = RepoUrl::parse(&srv.url("/trunk")).unwrap();
        tokio::spawn(async move {
            connector
                .connect(&url, &CancelToken::new())
                .await
                .map(|_| ())
        })
    };
    let first = app.respond(0, credentials("typo", true)).await;
    assert_eq!(first.reason, Some(opensvn_ipc::AuthPromptReason::Initial));
    let second = app.respond(1, AuthResponse::Cancel).await;
    assert_eq!(second.reason, Some(opensvn_ipc::AuthPromptReason::Rejected));
    assert_eq!(
        connecting.await.unwrap().err().unwrap().code,
        ErrorCode::AuthRequired
    );
    assert!(store.get(&origin, USER).unwrap().is_none(), "never saved");
    assert!(db.accounts_list().unwrap().is_empty());

    // A rebuilt transport (fresh attempt count) still tells the user why it asks again.
    let (_tx, rx) = watch::channel(crate::settings::defaults(None));
    let rebuilt = connector_for(&app.broker, rx);
    let connecting = {
        let url = RepoUrl::parse(&srv.url("/trunk")).unwrap();
        tokio::spawn(async move { rebuilt.connect(&url, &CancelToken::new()).await.map(|_| ()) })
    };
    let third = app.respond(2, credentials(PASSWORD, true)).await;
    assert_eq!(third.reason, Some(opensvn_ipc::AuthPromptReason::Rejected));
    assert_eq!(third.attempt, 0);
    connecting.await.unwrap().unwrap();
    // Accepted by the server, so saved now.
    assert!(store.get(&origin, USER).unwrap().is_some());
    assert_eq!(db.accounts_list().unwrap().len(), 1);

    // And used silently after a restart, also by background work.
    let restarted = start(&db, &store);
    restarted
        .connect(&srv.url("/trunk"), Anchor::Url, false)
        .await
        .unwrap()
        .ensure_authenticated(&CancelToken::new())
        .await
        .unwrap();
    assert!(restarted.prompts().is_empty());
}

#[tokio::test]
async fn a_stale_saved_password_stays_withheld_after_a_restart() {
    use secrecy::ExposeSecret;
    use secure_store::CredentialStore;
    let srv = SvnServer::start(Script {
        require_auth: true,
        ..Script::default()
    })
    .await;
    let (db, store) = (AppDb::open_in_memory().unwrap(), store());
    trust(&db, &srv);
    db.account_upsert(&crate::db::AccountRecord {
        scheme: "https",
        host: "127.0.0.1",
        port: srv.port,
        realm: REALM,
        username: USER,
        negotiate: false,
    })
    .unwrap();
    let origin = test_origin(&srv);
    store
        .set(
            &origin,
            USER,
            &secrecy::SecretString::from("old".to_owned()),
        )
        .unwrap();
    {
        let first = start(&db, &store);
        let err = first
            .connect(&srv.url("/trunk"), Anchor::Url, false)
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::AuthRequired);
        assert_eq!(rejected_submissions(&srv), 1);
    }
    let account = db
        .account_get("https", "127.0.0.1", srv.port, REALM)
        .unwrap()
        .unwrap();
    assert!(account.stale, "persisted");

    // Restart: the stale password is not sent by background work...
    let second = start(&db, &store);
    let err = second
        .connect(&srv.url("/trunk"), Anchor::Url, false)
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, ErrorCode::AuthRequired);
    assert_eq!(rejected_submissions(&srv), 1, "not submitted again");
    assert!(second.prompts().is_empty());
    // ...the user is asked instead, told why, and the old password is still in the store.
    let connecting = {
        let connector = second.connector.clone();
        let url = RepoUrl::parse(&srv.url("/trunk")).unwrap();
        tokio::spawn(async move {
            connector
                .connect(&url, &CancelToken::new())
                .await
                .map(|_| ())
        })
    };
    let prompt = second.respond(0, AuthResponse::Cancel).await;
    assert_eq!(prompt.reason, Some(opensvn_ipc::AuthPromptReason::Expired));
    assert!(connecting.await.unwrap().is_err());
    assert_eq!(rejected_submissions(&srv), 1);
    assert_eq!(
        store.get(&origin, USER).unwrap().unwrap().expose_secret(),
        "old",
        "kept, only no longer offered"
    );

    // Entering it again (saved) clears the flag. (A new transport: this one remembers the
    // cancelled prompt.)
    let answering = second.answer_prompts(true, IpcDecision::Reject);
    let (_tx, rx) = watch::channel(crate::settings::defaults(None));
    connector_for(&second.broker, rx)
        .connect(
            &RepoUrl::parse(&srv.url("/trunk")).unwrap(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    answering.abort();
    let account = db
        .account_get("https", "127.0.0.1", srv.port, REALM)
        .unwrap()
        .unwrap();
    assert!(!account.stale);
    let third = start(&db, &store);
    third
        .connect(&srv.url("/trunk"), Anchor::Url, false)
        .await
        .unwrap()
        .ensure_authenticated(&CancelToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_timed_out_sign_in_prompt_does_not_block_the_next_attempt() {
    // Issue #108: an unanswered prompt (broker timeout) used to fail every later request on the
    // shared transport without asking again.
    let srv = SvnServer::start(Script {
        require_auth: true,
        ..Script::default()
    })
    .await;
    let (db, store) = (AppDb::open_in_memory().unwrap(), store());
    trust(&db, &srv);
    let app = start_with_timeout(&db, &store, Duration::from_millis(300));
    let err = app
        .connect(&srv.url("/trunk"), Anchor::Url, true)
        .await
        .err()
        .expect("nobody answered");
    assert_eq!(err.code, ErrorCode::AuthRequired);
    assert_eq!(app.prompts().len(), 1);

    // Background work still fails fast and never prompts.
    let err = app
        .connect(&srv.url("/trunk"), Anchor::Url, false)
        .await
        .err()
        .expect("background work needs the user");
    assert_eq!(err.code, ErrorCode::AuthRequired);
    assert_eq!(app.prompts().len(), 1);

    // The user tries again: a new prompt, answered this time.
    let connector = app.connector.clone();
    let url = RepoUrl::parse(&srv.url("/trunk")).unwrap();
    let retry = tokio::spawn(async move {
        connector
            .connect_with(
                &url,
                &ConnectOptions {
                    anchor: Anchor::Url,
                    interactive: true,
                },
                &CancelToken::new(),
            )
            .await
            .map(|_| ())
    });
    app.respond(1, credentials(PASSWORD, false)).await;
    retry
        .await
        .unwrap()
        .expect("signed in on the second prompt");
    assert_eq!(app.prompts().len(), 2);
}

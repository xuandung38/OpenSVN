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

use super::https_svn::{PASSWORD, REALM, ROOT, Script, SvnServer, USER};
use super::support::{fixture_on, standard_repo};
use crate::auth::AuthBroker;
use crate::db::AppDb;
use crate::engine::real::{
    Anchor, ConnectOptions, HttpConnector, SessionConnector, UnwiredRecovery,
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
    let events = Arc::new(RecordingEvents::default());
    let broker = Arc::new(AuthBroker::with_timeout(
        events.clone(),
        db.clone(),
        store.clone(),
        Duration::from_secs(20),
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
        Arc::new(UnwiredRecovery),
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

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use opensvn_ipc::{AuthPromptKind, AuthPromptReason, AuthResponse, CertDecision as IpcDecision};
use svn_ra_api::{
    AuthMethod, AuthPrompter, CertDecision, CertFailure, CertInfo, CredentialRequest, PromptReason,
};
use svn_types::{OriginKey, Timestamp};

use super::broker::PROMPT_TIMEOUT;
use super::*;
use crate::db::AppDb;
use crate::ops::events::{Recorded, RecordingEvents};

fn origin() -> OriginKey {
    OriginKey {
        scheme: "https".into(),
        host: "svn.test.local".into(),
        port: 8443,
        realm: "<https://svn.test.local:8443> Repo".into(),
    }
}

fn setup(timeout: Duration) -> (Arc<AuthBroker>, Arc<RecordingEvents>, AppDb) {
    let events = Arc::new(RecordingEvents::default());
    let db = AppDb::open_in_memory().unwrap();
    let broker = Arc::new(AuthBroker::with_timeout(
        events.clone(),
        db.clone(),
        Arc::new(secure_store::MemoryStore::new()),
        timeout,
    ));
    (broker, events, db)
}

async fn next_prompt(events: &RecordingEvents) -> opensvn_ipc::AuthPrompt {
    next_prompt_after(events, 0).await
}

/// The prompt number `already + 1` (prompts are numbered in emission order).
async fn next_prompt_after(events: &RecordingEvents, already: usize) -> opensvn_ipc::AuthPrompt {
    for _ in 0..500 {
        let prompts: Vec<_> = events
            .all()
            .into_iter()
            .filter_map(|r| {
                if let Recorded::Auth(p) = r {
                    Some(p)
                } else {
                    None
                }
            })
            .collect();
        if let Some(p) = prompts.get(already) {
            return p.clone();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("no prompt");
}

fn cert(failure: CertFailure) -> CertInfo {
    CertInfo {
        host: "svn.test.local".into(),
        port: 8443,
        subject: "CN=svn".into(),
        issuer: "CN=ca".into(),
        not_before: Timestamp(0),
        not_after: Timestamp(4_000_000_000_000_000),
        sha256: [0xab; 32],
        failure,
    }
}

#[tokio::test]
async fn credentials_prompt_carries_origin_reason_and_returns_the_answer() {
    let (broker, events, db) = setup(PROMPT_TIMEOUT);
    let req = CredentialRequest::new(origin(), 1, vec![AuthMethod::Basic], PromptReason::Rejected);
    let b = broker.clone();
    let task = tokio::spawn(async move { b.credentials(&req).await });
    let prompt = next_prompt(&events).await;
    assert_eq!(prompt.kind, AuthPromptKind::Credentials);
    assert_eq!(
        prompt.url, "https://svn.test.local:8443",
        "full host:port, not just the realm"
    );
    assert_eq!(prompt.reason, Some(AuthPromptReason::Rejected));
    assert_eq!(prompt.attempt, 1);
    broker
        .respond(
            &prompt.request_id,
            AuthResponse::Credentials {
                username: "alice".into(),
                password: "s3cret".to_owned().into(),
                save: true,
            },
        )
        .unwrap();
    let creds = task.await.unwrap().unwrap();
    assert_eq!(creds.username, "alice");
    assert_eq!(creds.method, AuthMethod::Basic);
    assert!(creds.save);
    // Nothing is saved before the server accepted the password.
    assert!(db.accounts_list().unwrap().is_empty());
    broker.credentials_accepted(&origin(), &creds).await;
    // metadata only, keyed by origin
    let accounts = db.accounts_list().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].username.as_deref(), Some("alice"));
    assert_eq!(broker.waiting(), 0);
}

#[tokio::test]
async fn cancel_and_timeout_mean_no_credentials() {
    let (broker, events, _) = setup(PROMPT_TIMEOUT);
    let req = CredentialRequest::new(origin(), 0, vec![AuthMethod::Basic], PromptReason::Initial);
    let b = broker.clone();
    let r = req.clone();
    let task = tokio::spawn(async move { b.credentials(&r).await });
    let prompt = next_prompt(&events).await;
    broker
        .respond(&prompt.request_id, AuthResponse::Cancel)
        .unwrap();
    assert!(task.await.unwrap().is_none());

    let (quick, _, _) = setup(Duration::from_millis(30));
    assert!(quick.credentials(&req).await.is_none());
    assert_eq!(quick.waiting(), 0, "timed-out prompt is forgotten");
    assert!(quick.respond("gone", AuthResponse::Cancel).is_err());
}

#[tokio::test]
async fn answer_of_the_wrong_kind_is_refused_and_the_prompt_keeps_waiting() {
    let (broker, events, _) = setup(PROMPT_TIMEOUT);
    let b = broker.clone();
    let task =
        tokio::spawn(async move { b.confirm_certificate(&cert(CertFailure::SelfSigned)).await });
    let prompt = next_prompt(&events).await;
    let wrong = AuthResponse::Credentials {
        username: "x".into(),
        password: "y".to_owned().into(),
        save: false,
    };
    assert!(broker.respond(&prompt.request_id, wrong).is_err());
    assert_eq!(broker.waiting(), 1);
    broker
        .respond(
            &prompt.request_id,
            AuthResponse::Certificate {
                decision: IpcDecision::Reject,
            },
        )
        .unwrap();
    assert_eq!(task.await.unwrap(), CertDecision::Reject);
}

#[tokio::test]
async fn always_trust_is_remembered_per_host_port_and_fingerprint() {
    let (broker, events, db) = setup(PROMPT_TIMEOUT);
    let b = broker.clone();
    let task = tokio::spawn(async move {
        b.confirm_certificate(&cert(CertFailure::UnknownIssuer))
            .await
    });
    let prompt = next_prompt(&events).await;
    let summary = prompt.cert.clone().unwrap();
    assert_eq!(summary.host, "svn.test.local:8443");
    assert_eq!(summary.sha256, "ab".repeat(32));
    assert_eq!(summary.failure, opensvn_ipc::CertFailureInfo::UnknownIssuer);
    broker
        .respond(
            &prompt.request_id,
            AuthResponse::Certificate {
                decision: IpcDecision::AcceptAlways,
            },
        )
        .unwrap();
    assert_eq!(task.await.unwrap(), CertDecision::AcceptAlways);
    assert!(
        db.cert_is_trusted("svn.test.local", 8443, &"ab".repeat(32))
            .unwrap()
    );
    assert!(
        !db.cert_is_trusted("svn.test.local", 443, &"ab".repeat(32))
            .unwrap(),
        "other port"
    );

    // a remembered certificate is accepted without another prompt
    let before = events.all().len();
    assert_eq!(
        broker
            .confirm_certificate(&cert(CertFailure::UnknownIssuer))
            .await,
        CertDecision::AcceptAlways
    );
    assert_eq!(events.all().len(), before);
}

#[tokio::test]
async fn decisions_that_do_not_fit_the_failure_are_rejected_server_side() {
    let (broker, events, db) = setup(PROMPT_TIMEOUT);
    let cases = [
        (
            CertFailure::Changed {
                old_sha256: [1; 32],
                new_sha256: [0xab; 32],
            },
            IpcDecision::AcceptAlways,
        ),
        (CertFailure::Expired, IpcDecision::AcceptAlways),
        (CertFailure::HostnameMismatch, IpcDecision::AcceptAlways),
        (CertFailure::UnknownIssuer, IpcDecision::ReplacePin),
        (CertFailure::SelfSigned, IpcDecision::ReplacePin),
        (CertFailure::Expired, IpcDecision::ReplacePin),
        (CertFailure::Expired, IpcDecision::AcceptOnce),
        (CertFailure::HostnameMismatch, IpcDecision::AcceptOnce),
        (CertFailure::Revoked, IpcDecision::AcceptOnce),
        (
            CertFailure::Changed {
                old_sha256: [1; 32],
                new_sha256: [0xab; 32],
            },
            IpcDecision::AcceptOnce,
        ),
    ];
    for (asked, (failure, decision)) in cases.into_iter().enumerate() {
        let b = broker.clone();
        let f = failure.clone();
        let task = tokio::spawn(async move { b.confirm_certificate(&cert(f)).await });
        let prompt = next_prompt_after(&events, asked).await;
        if matches!(failure, CertFailure::Changed { .. }) {
            assert_eq!(
                prompt.cert.as_ref().unwrap().failure,
                opensvn_ipc::CertFailureInfo::Changed {
                    old_sha256: "01".repeat(32),
                    new_sha256: "ab".repeat(32),
                }
            );
        }
        broker
            .respond(&prompt.request_id, AuthResponse::Certificate { decision })
            .unwrap();
        assert_eq!(
            task.await.unwrap(),
            CertDecision::Reject,
            "{failure:?} + {decision:?}"
        );
        assert!(
            !db.cert_is_trusted("svn.test.local", 8443, &"ab".repeat(32))
                .unwrap()
        );
    }
}

#[tokio::test]
async fn replace_pin_swaps_the_pinned_fingerprint_for_a_changed_certificate_only() {
    let (broker, events, db) = setup(PROMPT_TIMEOUT);
    db.cert_trust(
        "svn.test.local",
        8443,
        &"01".repeat(32),
        "CN=old",
        "2030-01-01T00:00:00Z",
    )
    .unwrap();
    db.cert_trust(
        "svn.test.local",
        443,
        &"02".repeat(32),
        "CN=other-port",
        "2030-01-01T00:00:00Z",
    )
    .unwrap();
    let b = broker.clone();
    let task = tokio::spawn(async move {
        b.confirm_certificate(&cert(CertFailure::Changed {
            old_sha256: [1; 32],
            new_sha256: [0xab; 32],
        }))
        .await
    });
    let prompt = next_prompt(&events).await;
    broker
        .respond(
            &prompt.request_id,
            AuthResponse::Certificate {
                decision: IpcDecision::ReplacePin,
            },
        )
        .unwrap();
    assert_eq!(
        task.await.unwrap(),
        CertDecision::ReplacePin,
        "the transport receives ReplacePin, the only decision it accepts for a changed certificate"
    );
    assert!(
        db.cert_is_trusted("svn.test.local", 8443, &"ab".repeat(32))
            .unwrap()
    );
    assert!(
        !db.cert_is_trusted("svn.test.local", 8443, &"01".repeat(32))
            .unwrap(),
        "old pin is gone"
    );
    assert!(
        db.cert_is_trusted("svn.test.local", 443, &"02".repeat(32))
            .unwrap(),
        "other ports untouched"
    );
}

#[tokio::test]
async fn sign_out_removes_accounts_of_the_realm_and_purges_secrets() {
    use secure_store::CredentialStore;
    let store = Arc::new(secure_store::MemoryStore::new());
    let events = Arc::new(RecordingEvents::default());
    let db = AppDb::open_in_memory().unwrap();
    let broker = AuthBroker::new(events, db.clone(), store.clone());
    db.account_upsert(&crate::db::AccountRecord {
        scheme: "https",
        host: "h",
        port: 443,
        realm: "r",
        username: "u",
        negotiate: false,
    })
    .unwrap();
    let origin = OriginKey {
        scheme: "https".into(),
        host: "h".into(),
        port: 443,
        realm: "r".into(),
    };
    store
        .set(&origin, "u", &secrecy::SecretString::from("pw".to_owned()))
        .unwrap();
    let invalidations = broker.invalidations();
    broker.sign_out("r").unwrap();
    assert!(db.accounts_list().unwrap().is_empty());
    assert!(store.get(&origin, "u").unwrap().is_none(), "secret purged");
    assert!(
        invalidations.has_changed().unwrap(),
        "cached sessions are dropped"
    );
}

/// Answers the next credentials prompt with `user`/`pass`.
async fn answer(
    broker: &AuthBroker,
    events: &RecordingEvents,
    nth: usize,
    user: &str,
    pass: &str,
    save: bool,
) {
    let prompt = next_prompt_after(events, nth).await;
    broker
        .respond(
            &prompt.request_id,
            AuthResponse::Credentials {
                username: user.into(),
                password: pass.to_owned().into(),
                save,
            },
        )
        .unwrap();
}

#[tokio::test]
async fn saved_credentials_are_offered_silently_by_a_new_broker() {
    use secrecy::ExposeSecret;
    let store = Arc::new(secure_store::MemoryStore::new());
    let db = AppDb::open_in_memory().unwrap();
    let events = Arc::new(RecordingEvents::default());
    let first = Arc::new(AuthBroker::new(events.clone(), db.clone(), store.clone()));
    let req = CredentialRequest::new(origin(), 0, vec![AuthMethod::Basic], PromptReason::Initial);
    assert!(first.stored_credentials(&req).await.is_none());
    let (b, r) = (first.clone(), req.clone());
    let task = tokio::spawn(async move { b.credentials(&r).await });
    answer(&first, &events, 0, "alice", "s3cret", true).await;
    let typed = task.await.unwrap().unwrap();
    assert!(typed.save);
    first.credentials_accepted(&origin(), &typed).await;

    // "Restart": a new broker over the same database and credential store.
    let quiet = Arc::new(RecordingEvents::default());
    let second = AuthBroker::new(quiet.clone(), db, store);
    let creds = second.stored_credentials(&req).await.expect("saved");
    assert_eq!(creds.username, "alice");
    assert_eq!(creds.password.expose_secret(), "s3cret");
    assert_eq!(creds.method, AuthMethod::Basic);
    assert!(quiet.all().is_empty(), "no prompt for saved credentials");
}

#[tokio::test]
async fn unsaved_answers_store_nothing() {
    let store = Arc::new(secure_store::MemoryStore::new());
    let db = AppDb::open_in_memory().unwrap();
    let events = Arc::new(RecordingEvents::default());
    let broker = Arc::new(AuthBroker::new(events.clone(), db.clone(), store));
    let req = CredentialRequest::new(origin(), 0, vec![AuthMethod::Basic], PromptReason::Initial);
    let (b, r) = (broker.clone(), req.clone());
    let task = tokio::spawn(async move { b.credentials(&r).await });
    answer(&broker, &events, 0, "alice", "pw", false).await;
    task.await.unwrap().unwrap();
    assert!(broker.stored_credentials(&req).await.is_none());
    assert!(db.accounts_list().unwrap().is_empty());
}

/// Asks `broker` for credentials with `req`, answers the prompt number `nth` and returns what
/// the transport would get.
async fn typed(
    broker: &Arc<AuthBroker>,
    events: &RecordingEvents,
    req: &CredentialRequest,
    nth: usize,
    pass: &str,
    save: bool,
) -> (opensvn_ipc::AuthPrompt, svn_ra_api::Credentials) {
    let (b, r) = (broker.clone(), req.clone());
    let task = tokio::spawn(async move { b.credentials(&r).await });
    let prompt = next_prompt_after(events, nth).await;
    answer(broker, events, nth, "alice", pass, save).await;
    (prompt, task.await.unwrap().unwrap())
}

#[tokio::test]
async fn a_rejected_saved_password_is_not_offered_again_until_new_credentials_are_saved() {
    use secrecy::ExposeSecret;
    let store = Arc::new(secure_store::MemoryStore::new());
    let db = AppDb::open_in_memory().unwrap();
    let events = Arc::new(RecordingEvents::default());
    let broker = Arc::new(AuthBroker::new(events.clone(), db.clone(), store.clone()));
    let initial =
        CredentialRequest::new(origin(), 0, vec![AuthMethod::Basic], PromptReason::Initial);
    let (_, creds) = typed(&broker, &events, &initial, 0, "old", true).await;
    broker.credentials_accepted(&origin(), &creds).await;

    // The transport offers the saved password, the server rejects it (reported through the
    // hook), the user is prompted and cancels.
    assert!(broker.stored_credentials(&initial).await.is_some());
    let rejected =
        CredentialRequest::new(origin(), 1, vec![AuthMethod::Basic], PromptReason::Rejected);
    broker.stored_credentials_rejected(&rejected).await;
    let (b, r) = (broker.clone(), rejected.clone());
    let task = tokio::spawn(async move { b.credentials(&r).await });
    let prompt = next_prompt_after(&events, 1).await;
    broker
        .respond(&prompt.request_id, AuthResponse::Cancel)
        .unwrap();
    assert!(task.await.unwrap().is_none());
    assert!(
        broker.stored_credentials(&initial).await.is_none(),
        "stale password withheld"
    );
    // Persisted: a new broker (restart) withholds it too and says why it asks.
    let restarted = Arc::new(AuthBroker::new(events.clone(), db.clone(), store.clone()));
    assert!(restarted.stored_credentials(&initial).await.is_none());
    let (prompt, _) = typed(&restarted, &events, &initial, 2, "x", false).await;
    assert_eq!(prompt.reason, Some(AuthPromptReason::Expired));
    let account = db
        .account_get("https", "svn.test.local", 8443, &origin().realm)
        .unwrap()
        .unwrap();
    assert!(account.stale);
    use secure_store::CredentialStore;
    assert_eq!(
        store
            .get(&origin(), "alice")
            .unwrap()
            .unwrap()
            .expose_secret(),
        "old",
        "kept in the store"
    );

    // An unsaved accepted answer leaves the stale one withheld; a saved accepted one replaces it.
    let (_, creds) = typed(&broker, &events, &initial, 3, "typed", false).await;
    broker.credentials_accepted(&origin(), &creds).await;
    assert!(broker.stored_credentials(&initial).await.is_none());
    let (_, creds) = typed(&broker, &events, &initial, 4, "new", true).await;
    assert!(
        broker.stored_credentials(&initial).await.is_none(),
        "not before the server accepted it"
    );
    broker.credentials_accepted(&origin(), &creds).await;
    let creds = broker
        .stored_credentials(&initial)
        .await
        .expect("offered again");
    assert_eq!(creds.password.expose_secret(), "new");
    assert!(
        restarted.stored_credentials(&initial).await.is_some(),
        "the persisted flag is cleared too"
    );
}

#[tokio::test]
async fn a_typed_password_the_server_rejects_is_never_saved_and_the_next_prompt_says_so() {
    let (broker, events, db) = setup(PROMPT_TIMEOUT);
    let initial =
        CredentialRequest::new(origin(), 0, vec![AuthMethod::Basic], PromptReason::Initial);
    let (prompt, creds) = typed(&broker, &events, &initial, 0, "typo", true).await;
    assert_eq!(prompt.reason, Some(AuthPromptReason::Initial));
    assert!(creds.save);
    // The server rejects it: the transport prompts again (attempt 1); the user cancels.
    let again =
        CredentialRequest::new(origin(), 1, vec![AuthMethod::Basic], PromptReason::Rejected);
    let (b, r) = (broker.clone(), again.clone());
    let task = tokio::spawn(async move { b.credentials(&r).await });
    let prompt = next_prompt_after(&events, 1).await;
    assert_eq!(prompt.reason, Some(AuthPromptReason::Rejected));
    broker
        .respond(&prompt.request_id, AuthResponse::Cancel)
        .unwrap();
    assert!(task.await.unwrap().is_none());
    assert!(db.accounts_list().unwrap().is_empty(), "never saved");
    assert!(broker.stored_credentials(&initial).await.is_none());
    // A later first prompt (new transport, attempt 0) still says the last password was rejected.
    let (prompt, creds) = typed(&broker, &events, &initial, 2, "s3cret", true).await;
    assert_eq!(prompt.reason, Some(AuthPromptReason::Rejected));
    assert_eq!(prompt.attempt, 0);
    broker.credentials_accepted(&origin(), &creds).await;
    let (prompt, _) = typed(&broker, &events, &initial, 3, "s3cret", false).await;
    assert_eq!(
        prompt.reason,
        Some(AuthPromptReason::Initial),
        "accepted since"
    );
}

#[tokio::test]
async fn a_password_typed_before_a_sign_out_is_not_saved_by_a_late_response() {
    use secure_store::CredentialStore;
    let store = Arc::new(secure_store::MemoryStore::new());
    let db = AppDb::open_in_memory().unwrap();
    let events = Arc::new(RecordingEvents::default());
    let broker = Arc::new(AuthBroker::new(events.clone(), db.clone(), store.clone()));
    let initial =
        CredentialRequest::new(origin(), 0, vec![AuthMethod::Basic], PromptReason::Initial);
    let (_, creds) = typed(&broker, &events, &initial, 0, "pw", true).await;
    // The user signs out while the request carrying the answer is still in flight.
    broker.sign_out(&origin().realm).unwrap();
    broker.credentials_accepted(&origin(), &creds).await;
    assert!(db.accounts_list().unwrap().is_empty(), "sign-out stands");
    assert!(store.get(&origin(), "alice").unwrap().is_none());
    assert!(broker.stored_credentials(&initial).await.is_none());

    // An answer typed after the sign-out is saved as usual.
    let (_, creds) = typed(&broker, &events, &initial, 1, "pw", true).await;
    broker.credentials_accepted(&origin(), &creds).await;
    assert_eq!(db.accounts_list().unwrap().len(), 1);
    assert!(broker.stored_credentials(&initial).await.is_some());

    // A sign-out of another realm does not affect it.
    let (_, creds) = typed(&broker, &events, &initial, 2, "pw2", true).await;
    broker.sign_out("some other realm").unwrap();
    broker.credentials_accepted(&origin(), &creds).await;
    use secrecy::ExposeSecret;
    assert_eq!(
        store
            .get(&origin(), "alice")
            .unwrap()
            .unwrap()
            .expose_secret(),
        "pw2"
    );
}

/// A credential store whose `set` signals that it started and then waits to be released.
struct GatedStore {
    inner: secure_store::MemoryStore,
    entered: std::sync::atomic::AtomicBool,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl secure_store::CredentialStore for GatedStore {
    fn get(&self, o: &OriginKey, u: &str) -> secure_store::Result<Option<secrecy::SecretString>> {
        self.inner.get(o, u)
    }
    fn set(&self, o: &OriginKey, u: &str, p: &secrecy::SecretString) -> secure_store::Result<()> {
        self.entered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.release.lock().unwrap().recv();
        self.inner.set(o, u, p)
    }
    fn delete(&self, o: &OriginKey, u: &str) -> secure_store::Result<bool> {
        self.inner.delete(o, u)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sign_out_during_the_keychain_write_removes_the_saved_password() {
    use secure_store::CredentialStore;
    let (release, rx) = std::sync::mpsc::channel();
    let store = Arc::new(GatedStore {
        inner: secure_store::MemoryStore::new(),
        entered: Default::default(),
        release: std::sync::Mutex::new(rx),
    });
    let db = AppDb::open_in_memory().unwrap();
    let events = Arc::new(RecordingEvents::default());
    let broker = Arc::new(AuthBroker::new(events.clone(), db.clone(), store.clone()));
    let initial =
        CredentialRequest::new(origin(), 0, vec![AuthMethod::Basic], PromptReason::Initial);
    let (_, creds) = typed(&broker, &events, &initial, 0, "pw", true).await;

    // The server accepts the password; the keychain write starts and stalls.
    let b = broker.clone();
    let accepted = tokio::spawn(async move { b.credentials_accepted(&origin(), &creds).await });
    for _ in 0..500 {
        if store.entered.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(store.entered.load(std::sync::atomic::Ordering::SeqCst));
    // The user signs out meanwhile: it waits for the save to finish instead of interleaving.
    let b = broker.clone();
    let sign_out = std::thread::spawn(move || b.sign_out(&origin().realm));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !sign_out.is_finished(),
        "sign-out waits for the account gate"
    );
    // Neither the stalled write nor the waiting sign-out blocks a new prompt: it reads the
    // sign-out count, which is not the gate.
    let other = CredentialRequest::new(
        OriginKey {
            realm: "another realm".into(),
            ..origin()
        },
        0,
        vec![AuthMethod::Basic],
        PromptReason::Initial,
    );
    let prompt = tokio::time::timeout(
        Duration::from_secs(5),
        typed(&broker, &events, &other, 1, "pw2", false),
    )
    .await;
    if prompt.is_err() {
        release.send(()).unwrap();
        panic!("a prompt waited for the keychain write");
    }
    release.send(()).unwrap();
    accepted.await.unwrap();
    sign_out.join().unwrap().unwrap();

    // The sign-out stands: neither the secret nor the row survived.
    assert!(db.accounts_list().unwrap().is_empty(), "no account row");
    assert!(
        store.get(&origin(), "alice").unwrap().is_none(),
        "no secret"
    );
    assert!(broker.stored_credentials(&initial).await.is_none());
}

#[tokio::test]
async fn a_rejected_saved_password_is_not_worded_as_the_one_typed_last_time() {
    let store = Arc::new(secure_store::MemoryStore::new());
    let db = AppDb::open_in_memory().unwrap();
    let events = Arc::new(RecordingEvents::default());
    let broker = Arc::new(AuthBroker::new(events.clone(), db.clone(), store.clone()));
    let initial =
        CredentialRequest::new(origin(), 0, vec![AuthMethod::Basic], PromptReason::Initial);
    let (_, creds) = typed(&broker, &events, &initial, 0, "old", true).await;
    broker.credentials_accepted(&origin(), &creds).await;
    // A typed answer that never got a response (its request was cancelled)...
    let (_, _unanswered) = typed(&broker, &events, &initial, 1, "x", false).await;
    // ...then another transport offers the saved password and the server rejects it.
    assert!(broker.stored_credentials(&initial).await.is_some());
    let rejected =
        CredentialRequest::new(origin(), 1, vec![AuthMethod::Basic], PromptReason::Rejected);
    broker.stored_credentials_rejected(&rejected).await;
    let (b, r) = (broker.clone(), rejected.clone());
    let task = tokio::spawn(async move { b.credentials(&r).await });
    let prompt = next_prompt_after(&events, 2).await;
    assert_eq!(prompt.reason, Some(AuthPromptReason::Rejected));
    broker
        .respond(&prompt.request_id, AuthResponse::Cancel)
        .unwrap();
    assert!(task.await.unwrap().is_none());
    // The rejected password was the saved one: a later first prompt says so, not "the password
    // you entered last time".
    let (prompt, _) = typed(&broker, &events, &initial, 3, "y", false).await;
    assert_eq!(prompt.reason, Some(AuthPromptReason::Expired));
}

/// Credential store whose `delete` fails for one host.
struct FailingDelete {
    inner: secure_store::MemoryStore,
    host: &'static str,
}

impl secure_store::CredentialStore for FailingDelete {
    fn get(&self, o: &OriginKey, u: &str) -> secure_store::Result<Option<secrecy::SecretString>> {
        self.inner.get(o, u)
    }
    fn set(&self, o: &OriginKey, u: &str, p: &secrecy::SecretString) -> secure_store::Result<()> {
        self.inner.set(o, u, p)
    }
    fn delete(&self, o: &OriginKey, u: &str) -> secure_store::Result<bool> {
        if o.host == self.host {
            Err(secure_store::StoreError::Unavailable("locked".into()))
        } else {
            self.inner.delete(o, u)
        }
    }
}

#[tokio::test]
async fn sign_out_deletes_secrets_first_continues_past_errors_and_keeps_rows_of_kept_secrets() {
    use secure_store::CredentialStore;
    let store = Arc::new(FailingDelete {
        inner: secure_store::MemoryStore::new(),
        host: "bad",
    });
    let db = AppDb::open_in_memory().unwrap();
    let broker = AuthBroker::new(
        Arc::new(RecordingEvents::default()),
        db.clone(),
        store.clone(),
    );
    let at = |host: &str| OriginKey {
        scheme: "https".into(),
        host: host.into(),
        port: 443,
        realm: "r".into(),
    };
    for host in ["a", "bad", "c"] {
        db.account_upsert(&crate::db::AccountRecord {
            scheme: "https",
            host,
            port: 443,
            realm: "r",
            username: "u",
            negotiate: false,
        })
        .unwrap();
        store
            .set(
                &at(host),
                "u",
                &secrecy::SecretString::from("pw".to_owned()),
            )
            .unwrap();
    }
    let invalidations = broker.invalidations();
    let err = broker.sign_out("r").unwrap_err();
    assert!(
        err.detail
            .as_deref()
            .unwrap_or_default()
            .contains("bad:443"),
        "{err:?}"
    );
    assert!(invalidations.has_changed().unwrap());
    // The others are gone, secret and row; the failed one keeps its row (no orphaned secret)
    // and is no longer offered.
    for host in ["a", "c"] {
        assert!(store.get(&at(host), "u").unwrap().is_none());
        assert!(db.account_get("https", host, 443, "r").unwrap().is_none());
    }
    assert!(store.get(&at("bad"), "u").unwrap().is_some());
    let kept = db.account_get("https", "bad", 443, "r").unwrap().unwrap();
    assert!(kept.stale);
    let req = CredentialRequest::new(at("bad"), 0, vec![AuthMethod::Basic], PromptReason::Initial);
    assert!(broker.stored_credentials(&req).await.is_none());
}

#[tokio::test]
async fn forgetting_a_pin_held_only_in_memory_rebuilds_the_transport() {
    use svn_ra::transport::TrustStore;
    let (broker, _, db) = setup(PROMPT_TIMEOUT);
    let trust = broker.trust_store();
    trust.pin("svn.test.local", 8443, [0xcd; 32]);
    // As if the database write in `pin` had failed: the pin lives in memory only.
    let key = opensvn_ipc::TrustedCertKey {
        host: "svn.test.local".into(),
        port: 8443,
        sha256: "cd".repeat(32),
    };
    assert!(db.certs_remove(&key).unwrap());
    let inv = broker.invalidations();
    assert!(broker.forget_certificate(&key).unwrap());
    assert!(trust.pinned("svn.test.local", 8443).is_empty());
    assert!(inv.has_changed().unwrap(), "the transport is rebuilt");
}

#[tokio::test]
async fn forgetting_a_certificate_updates_the_trust_store_and_invalidates_sessions() {
    use svn_ra::transport::TrustStore;
    let (broker, _, db) = setup(PROMPT_TIMEOUT);
    db.cert_trust("svn.test.local", 8443, &"ab".repeat(32), "CN=svn", "")
        .unwrap();
    // Loaded at construction: a new broker sees the row.
    let broker2 = AuthBroker::new(
        Arc::new(RecordingEvents::default()),
        db.clone(),
        Arc::new(secure_store::MemoryStore::new()),
    );
    let trust = broker2.trust_store();
    assert_eq!(trust.pinned("SVN.test.local", 8443), vec![[0xab; 32]]);
    assert!(
        broker
            .trust_store()
            .pinned("svn.test.local", 8443)
            .is_empty()
    );
    let inv = broker2.invalidations();
    let key = opensvn_ipc::TrustedCertKey {
        host: "svn.test.local".into(),
        port: 8443,
        sha256: "ab".repeat(32),
    };
    assert!(broker2.forget_certificate(&key).unwrap());
    assert!(trust.pinned("svn.test.local", 8443).is_empty());
    assert!(inv.has_changed().unwrap());
    assert!(!broker2.forget_certificate(&key).unwrap());
}

#[test]
fn trust_store_pins_persist_and_replace() {
    use svn_ra::transport::TrustStore;
    let db = AppDb::open_in_memory().unwrap();
    let store = super::trust::AppDbTrustStore::load(db.clone()).unwrap();
    store.pin("Host.Example", 443, [1; 32]);
    store.pin("host.example", 443, [1; 32]);
    assert_eq!(store.pinned("host.example", 443), vec![[1; 32]]);
    assert!(
        db.cert_is_trusted("host.example", 443, &"01".repeat(32))
            .unwrap()
    );
    store.replace_pins("host.example", 443, [2; 32]);
    let reloaded = super::trust::AppDbTrustStore::load(db.clone()).unwrap();
    assert_eq!(reloaded.pinned("host.example", 443), vec![[2; 32]]);
    assert!(reloaded.pinned("host.example", 8443).is_empty());
}

#[tokio::test]
async fn offered_methods_reach_the_prompt_and_sso_needs_negotiate_to_be_offered() {
    let (broker, events, _) = setup(PROMPT_TIMEOUT);
    let req = CredentialRequest::new(
        origin(),
        0,
        vec![AuthMethod::Negotiate, AuthMethod::Basic],
        PromptReason::Initial,
    );
    let b = broker.clone();
    let r = req.clone();
    let task = tokio::spawn(async move { b.credentials(&r).await });
    let prompt = next_prompt(&events).await;
    assert_eq!(
        prompt.offered,
        vec![
            opensvn_ipc::AuthMethodKind::Negotiate,
            opensvn_ipc::AuthMethodKind::Basic
        ]
    );
    broker
        .respond(&prompt.request_id, AuthResponse::Negotiate)
        .unwrap();
    let creds = task.await.unwrap().unwrap();
    assert_eq!(creds.method, AuthMethod::Negotiate);
    assert!(creds.username.is_empty());

    // SSO that was never offered is treated like a cancel
    let basic_only =
        CredentialRequest::new(origin(), 0, vec![AuthMethod::Basic], PromptReason::Initial);
    let b = broker.clone();
    let task = tokio::spawn(async move { b.credentials(&basic_only).await });
    let prompt = next_prompt_after(&events, 1).await;
    broker
        .respond(&prompt.request_id, AuthResponse::Negotiate)
        .unwrap();
    assert!(task.await.unwrap().is_none());
}

#[test]
fn ipc_cert_decisions_map_to_transport_decisions_the_transport_accepts() {
    use super::broker::map_cert_decision;
    let changed = CertFailure::Changed {
        old_sha256: [1; 32],
        new_sha256: [2; 32],
    };
    let failures = [
        CertFailure::UnknownIssuer,
        CertFailure::SelfSigned,
        CertFailure::Expired,
        CertFailure::NotYetValid,
        CertFailure::HostnameMismatch,
        CertFailure::Revoked,
        CertFailure::Other("x".into()),
        changed.clone(),
    ];
    let all = [
        IpcDecision::Reject,
        IpcDecision::AcceptOnce,
        IpcDecision::AcceptAlways,
        IpcDecision::ReplacePin,
    ];
    for f in &failures {
        for d in all {
            let mapped = map_cert_decision(d, f);
            assert!(mapped.is_valid_for(f), "{d:?} on {f:?} gave {mapped:?}");
        }
    }
    assert_eq!(
        map_cert_decision(IpcDecision::ReplacePin, &changed),
        CertDecision::ReplacePin
    );
    assert_eq!(
        map_cert_decision(IpcDecision::AcceptOnce, &CertFailure::SelfSigned),
        CertDecision::AcceptOnce
    );
    assert_eq!(
        map_cert_decision(IpcDecision::AcceptAlways, &CertFailure::UnknownIssuer),
        CertDecision::AcceptAlways
    );
    assert_eq!(
        map_cert_decision(IpcDecision::AcceptOnce, &changed),
        CertDecision::Reject
    );
    assert_eq!(
        map_cert_decision(IpcDecision::ReplacePin, &CertFailure::SelfSigned),
        CertDecision::Reject
    );
}

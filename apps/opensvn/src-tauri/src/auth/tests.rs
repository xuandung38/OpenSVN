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
    assert!(task.await.unwrap().unwrap().save);

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

#[tokio::test]
async fn a_rejected_saved_password_is_not_offered_again_until_new_credentials_are_saved() {
    use secrecy::ExposeSecret;
    let store = Arc::new(secure_store::MemoryStore::new());
    let db = AppDb::open_in_memory().unwrap();
    let events = Arc::new(RecordingEvents::default());
    let broker = Arc::new(AuthBroker::new(events.clone(), db.clone(), store));
    let initial =
        CredentialRequest::new(origin(), 0, vec![AuthMethod::Basic], PromptReason::Initial);
    let (b, r) = (broker.clone(), initial.clone());
    let task = tokio::spawn(async move { b.credentials(&r).await });
    answer(&broker, &events, 0, "alice", "old", true).await;
    task.await.unwrap().unwrap();

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

    // An unsaved answer leaves the stale one withheld; a saved one replaces it.
    let (b, r) = (broker.clone(), initial.clone());
    let task = tokio::spawn(async move { b.credentials(&r).await });
    answer(&broker, &events, 2, "alice", "typed", false).await;
    task.await.unwrap().unwrap();
    assert!(broker.stored_credentials(&initial).await.is_none());
    let (b, r) = (broker.clone(), initial.clone());
    let task = tokio::spawn(async move { b.credentials(&r).await });
    answer(&broker, &events, 3, "alice", "new", true).await;
    task.await.unwrap().unwrap();
    let creds = broker
        .stored_credentials(&initial)
        .await
        .expect("offered again");
    assert_eq!(creds.password.expose_secret(), "new");
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

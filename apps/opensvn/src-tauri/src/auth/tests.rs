#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use opensvn_ipc::{AuthPromptKind, AuthPromptReason, AuthResponse, CertDecision as IpcDecision};
use svn_ra_api::{
    AuthMethod, AuthPrompter, CertDecision, CertFailure, CertInfo, CredentialRequest, PromptReason,
};
use svn_types::{OriginKey, Timestamp};

use super::broker::{CredentialPurge, PROMPT_TIMEOUT};
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
        Arc::new(NoSecretStore),
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
        CertDecision::AcceptOnce,
        "this connection proceeds once"
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
    struct Spy(std::sync::Mutex<Vec<(String, u16)>>);
    impl CredentialPurge for Spy {
        fn purge(
            &self,
            _: &str,
            host: &str,
            port: u16,
            _: &str,
        ) -> Result<(), opensvn_ipc::AppError> {
            self.0.lock().unwrap().push((host.to_owned(), port));
            Ok(())
        }
    }
    let spy = Arc::new(Spy(Default::default()));
    let events = Arc::new(RecordingEvents::default());
    let db = AppDb::open_in_memory().unwrap();
    let broker = AuthBroker::new(events, db.clone(), spy.clone());
    db.account_upsert(&crate::db::AccountRecord {
        scheme: "https",
        host: "h",
        port: 443,
        realm: "r",
        username: "u",
        negotiate: false,
    })
    .unwrap();
    broker.sign_out("r").unwrap();
    assert!(db.accounts_list().unwrap().is_empty());
    assert_eq!(*spy.0.lock().unwrap(), vec![("h".to_owned(), 443)]);
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

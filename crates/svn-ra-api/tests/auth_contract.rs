//! Shape of the credential request and certificate failure types.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use svn_ra_api::{AuthMethod, CertFailure, CredentialRequest, PromptReason};
use svn_types::OriginKey;

fn origin() -> OriginKey {
    OriginKey {
        scheme: "https".into(),
        host: "svn.test.local".into(),
        port: 443,
        realm: "<https://svn.test.local:443> repo".into(),
    }
}

#[test]
fn credential_request_carries_context() {
    let req = CredentialRequest::new(
        origin(),
        2,
        vec![AuthMethod::Negotiate, AuthMethod::Basic],
        PromptReason::NegotiateFailed,
    );
    assert_eq!(req.attempt, 2);
    assert_eq!(req.offered, [AuthMethod::Negotiate, AuthMethod::Basic]);
    assert_eq!(req.reason, PromptReason::NegotiateFailed);
    assert_eq!(req.clone(), req);
}

#[test]
fn changed_certificate_failure_keeps_both_fingerprints() {
    let f = CertFailure::Changed {
        old_sha256: [1; 32],
        new_sha256: [2; 32],
    };
    match f {
        CertFailure::Changed {
            old_sha256,
            new_sha256,
        } => {
            assert_ne!(old_sha256, new_sha256);
        }
        _ => panic!("expected Changed"),
    }
    assert_ne!(f, CertFailure::Other("the certificate changed".into()));
}

#[test]
fn decisions_are_validated_against_the_actual_failure() {
    use svn_ra_api::CertDecision::{AcceptAlways, AcceptOnce, Reject, ReplacePin};
    let changed = CertFailure::Changed {
        old_sha256: [1; 32],
        new_sha256: [2; 32],
    };
    // Changed: only Reject and ReplacePin.
    assert!(Reject.is_valid_for(&changed));
    assert!(ReplacePin.is_valid_for(&changed));
    assert!(!AcceptOnce.is_valid_for(&changed));
    assert!(!AcceptAlways.is_valid_for(&changed));

    for f in [CertFailure::UnknownIssuer, CertFailure::SelfSigned] {
        assert!(
            Reject.is_valid_for(&f) && AcceptOnce.is_valid_for(&f) && AcceptAlways.is_valid_for(&f)
        );
        assert!(!ReplacePin.is_valid_for(&f));
    }
    for f in [
        CertFailure::Expired,
        CertFailure::NotYetValid,
        CertFailure::HostnameMismatch,
        CertFailure::Revoked,
        CertFailure::Other("x".into()),
    ] {
        assert!(Reject.is_valid_for(&f), "{f:?}");
        assert!(!AcceptOnce.is_valid_for(&f), "{f:?}");
        assert!(!AcceptAlways.is_valid_for(&f), "{f:?}");
        assert!(!ReplacePin.is_valid_for(&f), "{f:?}");
    }
}

struct Decides(svn_ra_api::CertDecision);

#[async_trait::async_trait]
impl svn_ra_api::AuthPrompter for Decides {
    async fn credentials(&self, _: &CredentialRequest) -> Option<svn_ra_api::Credentials> {
        None
    }
    async fn confirm_certificate(&self, _: &svn_ra_api::CertInfo) -> svn_ra_api::CertDecision {
        self.0
    }
}

fn cert(failure: CertFailure) -> svn_ra_api::CertInfo {
    svn_ra_api::CertInfo {
        host: "svn.test.local".into(),
        port: 443,
        subject: "CN=svn.test.local".into(),
        issuer: "CN=test ca".into(),
        not_before: svn_types::Timestamp(0),
        not_after: svn_types::Timestamp(1),
        sha256: [2; 32],
        failure,
    }
}

#[tokio::test]
async fn fake_session_enforces_decision_validity_for_a_changed_certificate() {
    use std::sync::Arc;
    use svn_ra_api::fake::FakeRaSession;
    use svn_ra_api::{CertDecision::*, RaSession};
    use svn_types::{CancelToken, ErrorCode};

    let changed = || CertFailure::Changed {
        old_sha256: [1; 32],
        new_sha256: [2; 32],
    };
    for (decision, ok) in [
        (ReplacePin, true),
        (Reject, false),
        (AcceptOnce, false),
        (AcceptAlways, false),
    ] {
        let s = FakeRaSession::new("https://svn.test.local/r", "u")
            .expect("session")
            .with_certificate_check(cert(changed()), Arc::new(Decides(decision)));
        let r = s.ensure_authenticated(&CancelToken::new()).await;
        assert_eq!(r.is_ok(), ok, "{decision:?} for a changed certificate");
        if let Err(e) = r {
            assert_eq!(e.code, ErrorCode::TlsUntrusted);
        }
    }
    // ReplacePin for any other failure is refused, AcceptAlways works for a self-signed cert.
    let s = FakeRaSession::new("https://svn.test.local/r", "u")
        .expect("session")
        .with_certificate_check(cert(CertFailure::SelfSigned), Arc::new(Decides(ReplacePin)));
    assert!(s.ensure_authenticated(&CancelToken::new()).await.is_err());
    let s = FakeRaSession::new("https://svn.test.local/r", "u")
        .expect("session")
        .with_certificate_check(
            cert(CertFailure::SelfSigned),
            Arc::new(Decides(AcceptAlways)),
        );
    s.ensure_authenticated(&CancelToken::new())
        .await
        .expect("accepted");
}

/// Cancels the operation while the user is still looking at the prompt.
struct CancelsWhileAsking(svn_types::CancelToken);

#[async_trait::async_trait]
impl svn_ra_api::AuthPrompter for CancelsWhileAsking {
    async fn credentials(&self, _: &CredentialRequest) -> Option<svn_ra_api::Credentials> {
        None
    }
    async fn confirm_certificate(&self, _: &svn_ra_api::CertInfo) -> svn_ra_api::CertDecision {
        self.0.cancel();
        svn_ra_api::CertDecision::AcceptOnce
    }
}

#[tokio::test]
async fn fake_session_honors_cancellation_that_happens_during_the_prompt() {
    use std::sync::Arc;
    use svn_ra_api::RaSession;
    use svn_ra_api::fake::FakeRaSession;
    let token = svn_types::CancelToken::new();
    let s = FakeRaSession::new("https://svn.test.local/r", "u")
        .expect("session")
        .with_certificate_check(
            cert(CertFailure::SelfSigned),
            Arc::new(CancelsWhileAsking(token.clone())),
        );
    let err = s.ensure_authenticated(&token).await.expect_err("cancelled");
    assert_eq!(err.code, svn_types::ErrorCode::Cancelled);
}

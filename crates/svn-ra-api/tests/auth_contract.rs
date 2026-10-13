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

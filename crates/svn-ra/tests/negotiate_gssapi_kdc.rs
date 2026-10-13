//! Real Kerberos round trip through the GSSAPI backend against a local KDC (manual, ignored).
//!
//! Needs a KDC, a ticket for the client (`kinit`) and a keytab holding `HTTP/<host>` for the
//! acceptor side. With `KRB5_CONFIG`, `KRB5CCNAME` and `KRB5_KTNAME` pointing at such a setup:
//!
//! ```text
//! OPENSVN_KRB_HOST=svn.test.local \
//!   cargo test -p svn-ra --features negotiate-gssapi --test negotiate_gssapi_kdc -- --ignored
//! ```
#![cfg(all(unix, feature = "negotiate-gssapi"))]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use libgssapi::context::{SecurityContext as _, ServerCtx};
use libgssapi::credential::{Cred, CredUsage};
use svn_ra::auth::NegotiateStep;
use svn_ra::auth::negotiate::{check_kerberos_token, system_provider};

fn host() -> String {
    std::env::var("OPENSVN_KRB_HOST").unwrap_or_else(|_| "svn.test.local".to_owned())
}

fn acceptor() -> ServerCtx {
    let cred = Cred::acquire(None, None, CredUsage::Accept, None).expect("acceptor keytab");
    ServerCtx::new(Some(cred))
}

#[test]
#[ignore = "needs a KDC, a client ticket and an HTTP/<host> keytab"]
fn kerberos_round_trip_with_channel_bindings() {
    let provider = system_provider().expect("GSSAPI provider");
    let cb = b"tls-server-end-point:0123456789abcdef".as_slice();

    let (mut ctx, token) = provider.initiate(&host(), Some(cb)).expect("initiate");
    check_kerberos_token(&token).expect("SPNEGO token starts Kerberos");
    let mut server = acceptor();
    let reply = server
        .step(&token, Some(cb))
        .expect("acceptor accepts the token")
        .expect("mutual-authentication token");
    assert!(server.is_complete());
    assert_eq!(
        ctx.step(&reply).expect("AP-REP verified"),
        NegotiateStep::Complete
    );

    // Bindings for a different certificate are refused by the acceptor.
    let (_ctx, token) = provider
        .initiate(&host(), Some(b"tls-server-end-point:ffffffffffffffff"))
        .expect("initiate");
    assert!(acceptor().step(&token, Some(cb)).is_err());

    // A forged final token fails mutual authentication.
    let (mut ctx, token) = provider.initiate(&host(), None).expect("initiate");
    let mut server = acceptor();
    let mut reply = server
        .step(&token, None)
        .expect("accept")
        .expect("reply")
        .to_vec();
    let last = reply.len() - 1;
    reply[last] ^= 0xff;
    assert!(ctx.step(&reply).is_err());
}

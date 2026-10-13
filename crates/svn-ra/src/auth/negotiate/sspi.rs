//! SSPI backend (Windows): the `Negotiate` package through `cross-krb5`, Kerberos only.
//!
//! `cross-krb5` checks that the established context negotiated Kerberos (an NTLM fallback is an
//! error) and that mutual authentication was granted.

use cross_krb5::{ClientCtx, InitiateFlags, PendingClientCtx, Step};
use svn_types::Result;

use super::{NegotiateContext, NegotiateProvider, NegotiateStep, negotiate_failed};

/// Uses the logon session's credentials (domain single sign-on).
pub(super) struct SspiProvider;

struct SspiContext {
    pending: Option<PendingClientCtx>,
}

impl NegotiateProvider for SspiProvider {
    fn initiate(&self, host: &str) -> Result<(Box<dyn NegotiateContext>, Vec<u8>)> {
        // SPNEGO tokens as IIS / VisualSVN expect; mutual authentication stays required (the
        // default). Confidentiality is not used by HTTP authentication, so it is not demanded.
        let flags = InitiateFlags::NEGOTIATE_TOKEN | InitiateFlags::DISABLE_CONFIDENTIALITY;
        let (pending, token) = ClientCtx::new(flags, None, &format!("HTTP/{host}"), None)
            .map_err(|e| negotiate_failed(format!("cannot start Kerberos authentication: {e}")))?;
        let token = token.to_vec();
        Ok((
            Box::new(SspiContext {
                pending: Some(pending),
            }),
            token,
        ))
    }
}

impl NegotiateContext for SspiContext {
    fn step(&mut self, token: &[u8]) -> Result<NegotiateStep> {
        let pending = self
            .pending
            .take()
            .ok_or_else(|| negotiate_failed("Negotiate context already finished"))?;
        let step = pending
            .step(token)
            .map_err(|e| negotiate_failed(format!("Kerberos mutual authentication failed: {e}")))?;
        Ok(match step {
            Step::Finished(_) => NegotiateStep::Complete,
            Step::Continue((next, out)) => {
                let out = out.to_vec();
                self.pending = Some(next);
                NegotiateStep::Continue(out)
            }
        })
    }
}

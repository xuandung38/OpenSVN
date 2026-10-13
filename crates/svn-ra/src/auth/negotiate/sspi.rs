//! SSPI backend (Windows): the `Negotiate` package through `cross-krb5`, Kerberos only.
//!
//! Kerberos only, in two places:
//!
//! - Before sending: the first token is inspected ([`super::check_kerberos_token`]). When SSPI has
//!   no ticket for `HTTP/<host>` (unknown SPN, short name, no domain) it puts NTLM first in the
//!   SPNEGO token; that token is refused here and never sent. This covers servers that accept
//!   without returning a final token.
//! - After a final server token: `cross-krb5` queries `SECPKG_ATTR_NEGOTIATION_INFO` on the
//!   established context, rejects any package other than Kerberos, and checks that mutual
//!   authentication was granted. `cross-krb5` 0.5 does not expose `QueryContextAttributes` for a
//!   context that is still pending, so when the server sends no final token the token inspection
//!   above is the only check (NTLM cannot complete in one leg in any case).
//!
//! Channel bindings are handed to `cross-krb5`, which wraps them in `SEC_CHANNEL_BINDINGS`
//! (application data only) for `InitializeSecurityContext`.

use cross_krb5::{ClientCtx, InitiateFlags, PendingClientCtx, Step};
use svn_types::Result;

use super::{NegotiateContext, NegotiateProvider, NegotiateStep, negotiate_failed};

/// Uses the logon session's credentials (domain single sign-on).
pub(super) struct SspiProvider;

struct SspiContext {
    pending: Option<PendingClientCtx>,
}

impl NegotiateProvider for SspiProvider {
    fn initiate(
        &self,
        host: &str,
        channel_bindings: Option<&[u8]>,
    ) -> Result<(Box<dyn NegotiateContext>, Vec<u8>)> {
        // SPNEGO tokens as IIS / VisualSVN expect; mutual authentication stays required (the
        // default). Confidentiality is not used by HTTP authentication, so it is not demanded.
        let flags = InitiateFlags::NEGOTIATE_TOKEN | InitiateFlags::DISABLE_CONFIDENTIALITY;
        let (pending, token) =
            ClientCtx::new(flags, None, &format!("HTTP/{host}"), channel_bindings).map_err(
                |e| negotiate_failed(format!("cannot start Kerberos authentication: {e}")),
            )?;
        let token = token.to_vec();
        super::check_kerberos_token(&token)?;
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

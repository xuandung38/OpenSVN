//! GSSAPI backend (macOS/Linux): SPNEGO mechanism over the system Kerberos library.
//!
//! The target is the host-based service name `HTTP@<host>` (`GSS_C_NT_HOSTBASED_SERVICE`), where
//! `<host>` is the request URL host. The Kerberos library maps it to `HTTP/<host>@REALM`; with
//! MIT krb5 that mapping follows `dns_canonicalize_hostname` and `rdns` in krb5.conf (see
//! `docs/visualsvn-iwa-checklist.md`).
//!
//! Kerberos only: SPNEGO is used because IIS/VisualSVN expect SPNEGO tokens, and `libgssapi`
//! does not expose `gss_set_neg_mechs`, so the mechanism list cannot be narrowed up front.
//! Instead the first token is inspected ([`super::check_kerberos_token`]) and must be a Kerberos
//! token before it is sent, and a completed context must report the Kerberos mech OID.

use libgssapi::context::{ClientCtx, CtxFlags, SecurityContext as _};
use libgssapi::name::Name;
use libgssapi::oid::{GSS_MECH_SPNEGO, GSS_NT_HOSTBASED_SERVICE};
use svn_types::Result;

use super::{NegotiateContext, NegotiateProvider, NegotiateStep, negotiate_failed};

/// Uses the default credential cache (`kinit`) of the current user.
pub(super) struct GssapiProvider;

struct GssContext {
    ctx: ClientCtx,
    /// Channel bindings must be passed on every `gss_init_sec_context` call of a context.
    bindings: Option<Vec<u8>>,
}

fn gss_error(what: &str, e: &libgssapi::error::Error) -> svn_types::SvnError {
    negotiate_failed(format!("{what}: {e}"))
}

impl NegotiateProvider for GssapiProvider {
    fn initiate(
        &self,
        host: &str,
        channel_bindings: Option<&[u8]>,
    ) -> Result<(Box<dyn NegotiateContext>, Vec<u8>)> {
        // GSSAPI host-based service name `HTTP@host`, mapped to the principal `HTTP/host@REALM`.
        let target = Name::new(
            format!("HTTP@{host}").as_bytes(),
            Some(GSS_NT_HOSTBASED_SERVICE),
        )
        .map_err(|e| gss_error("invalid Kerberos service name", &e))?;
        let mut ctx = ClientCtx::new(
            None,
            target,
            CtxFlags::GSS_C_MUTUAL_FLAG,
            Some(GSS_MECH_SPNEGO),
        );
        let token = ctx
            .step(None, channel_bindings)
            .map_err(|e| gss_error("cannot start Kerberos authentication", &e))?
            .ok_or_else(|| negotiate_failed("GSSAPI produced no initial token"))?
            .to_vec();
        super::check_kerberos_token(&token)?;
        Ok((
            Box::new(GssContext {
                ctx,
                bindings: channel_bindings.map(<[u8]>::to_vec),
            }),
            token,
        ))
    }
}

impl NegotiateContext for GssContext {
    fn step(&mut self, token: &[u8]) -> Result<NegotiateStep> {
        let out = self
            .ctx
            .step(Some(token), self.bindings.as_deref())
            .map_err(|e| gss_error("Kerberos mutual authentication failed", &e))?;
        if !self.ctx.is_complete() {
            return Ok(NegotiateStep::Continue(
                out.map(|b| b.to_vec()).unwrap_or_default(),
            ));
        }
        // Context establishment only reports flags the mechanism actually granted.
        let granted = self
            .ctx
            .flags()
            .map_err(|e| gss_error("cannot inspect the Kerberos context", &e))?;
        if !granted.contains(CtxFlags::GSS_C_MUTUAL_FLAG) {
            return Err(negotiate_failed(
                "Kerberos context completed without mutual authentication",
            ));
        }
        // SPNEGO reports the mechanism it actually negotiated.
        let info = self
            .ctx
            .info()
            .map_err(|e| gss_error("cannot inspect the Kerberos context", &e))?;
        if !super::is_kerberos_oid(&info.mechanism) {
            return Err(negotiate_failed(format!(
                "the server negotiated {} instead of Kerberos, which OpenSVN does not use",
                info.mechanism
            )));
        }
        Ok(NegotiateStep::Complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a ticket for this made-up realm, GSSAPI fails cleanly with `NegotiateFailed`.
    #[test]
    fn initiate_without_ticket_fails_with_negotiate_failed() {
        match GssapiProvider.initiate("svn.opensvn-test.invalid", None) {
            Ok(_) => {} // a host with a matching ticket cache; nothing to assert
            Err(e) => assert_eq!(e.code, svn_types::ErrorCode::NegotiateFailed),
        }
    }
}

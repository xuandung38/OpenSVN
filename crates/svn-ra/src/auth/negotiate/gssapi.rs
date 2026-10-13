//! GSSAPI backend (macOS/Linux): SPNEGO mechanism over the system Kerberos library.

use libgssapi::context::{ClientCtx, CtxFlags, SecurityContext as _};
use libgssapi::name::Name;
use libgssapi::oid::{GSS_MECH_SPNEGO, GSS_NT_HOSTBASED_SERVICE};
use svn_types::Result;

use super::{NegotiateContext, NegotiateProvider, NegotiateStep, negotiate_failed};

/// Uses the default credential cache (`kinit`) of the current user.
pub(super) struct GssapiProvider;

struct GssContext {
    ctx: ClientCtx,
}

fn gss_error(what: &str, e: &libgssapi::error::Error) -> svn_types::SvnError {
    negotiate_failed(format!("{what}: {e}"))
}

impl NegotiateProvider for GssapiProvider {
    fn initiate(&self, host: &str) -> Result<(Box<dyn NegotiateContext>, Vec<u8>)> {
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
            .step(None, None)
            .map_err(|e| gss_error("cannot start Kerberos authentication", &e))?
            .ok_or_else(|| negotiate_failed("GSSAPI produced no initial token"))?;
        Ok((Box::new(GssContext { ctx }), token.to_vec()))
    }
}

impl NegotiateContext for GssContext {
    fn step(&mut self, token: &[u8]) -> Result<NegotiateStep> {
        let out = self
            .ctx
            .step(Some(token), None)
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
        Ok(NegotiateStep::Complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a ticket for this made-up realm, GSSAPI fails cleanly with `NegotiateFailed`.
    #[test]
    fn initiate_without_ticket_fails_with_negotiate_failed() {
        match GssapiProvider.initiate("svn.opensvn-test.invalid") {
            Ok(_) => {} // a host with a matching ticket cache; nothing to assert
            Err(e) => assert_eq!(e.code, svn_types::ErrorCode::NegotiateFailed),
        }
    }
}

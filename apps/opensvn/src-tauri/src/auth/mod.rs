//! Authentication broker: turns transport prompts into `auth://prompt` events and waits for
//! `auth_respond`. The broker never holds a working copy lock while it waits: callers
//! authenticate before taking the write path (`EngineFacade::ensure_authenticated`).
//!
//! Saved passwords are read from and written to the OS credential store; permanently trusted
//! certificates are served to the transport from memory by [`AppDbTrustStore`].

mod broker;
mod trust;

pub use broker::{AuthBroker, os_credential_store};

#[cfg(test)]
mod tests;

//! Authentication broker: turns transport prompts into `auth://prompt` events and waits for
//! `auth_respond`. The broker never holds a working copy lock while it waits: callers
//! authenticate before taking the write path (`EngineFacade::ensure_authenticated`).

mod broker;

pub use broker::{AuthBroker, NoSecretStore};

#[cfg(test)]
mod tests;

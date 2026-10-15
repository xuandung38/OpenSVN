//! Tests of the real engine, run against the in-memory repository and temporary working copies
//! created by `svn-wc`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod commit_tests;
mod conformance_recovery_tests;
mod conformance_tests;
mod connector_tests;
mod edit_tests;
mod https_svn;
mod local_tests;
mod recovery_tests;
mod remote_tests;
mod scaffold_tests;
mod support;
mod sync_tests;
mod watcher_latency_tests;
mod wc_status_tests;

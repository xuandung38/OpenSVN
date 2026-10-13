//! Tests of the real engine, run against the in-memory repository and temporary working copies
//! created by `svn-wc`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod conformance_recovery_tests;
mod edit_tests;
mod local_tests;
mod recovery_tests;
mod remote_tests;
mod scaffold_tests;
mod support;
mod sync_tests;
mod wc_status_tests;

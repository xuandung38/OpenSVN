//! Tests of the real engine, run against the in-memory repository and temporary working copies
//! created by `svn-wc`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod edit_tests;
mod recovery_tests;
mod remote_tests;
mod scaffold_tests;
mod support;
mod wc_status_tests;

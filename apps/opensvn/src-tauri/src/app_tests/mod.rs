//! Tests that drive the command handlers over the fake engine with a mock Tauri runtime.

mod commands_flow;
mod commands_office;
mod commands_paths;
mod commands_remote;
mod commands_repo_manager;
mod commands_settings;
#[cfg(all(feature = "real-engine", feature = "engine-conformance"))]
mod dav_resume;
mod engine_startup;
mod fixture;
mod security;

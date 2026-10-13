//! OpenSVN compat harness.
//!
//! Runs YAML scenarios against Dockerised Subversion 1.14.5 / 1.15.0 (Apache + mod_dav_svn
//! servers, svn CLI clients) and compares the resulting working copies: file tree (with exec
//! bit), `svn status --xml`, `svn info --xml`, and a normalised `wc.db` dump.
//!
//! Docker-dependent tests are `#[ignore]`d; run them with
//! `cargo xtask compat up && cargo xtask compat fixtures && cargo test -p opensvn-compat -- --ignored`.

pub mod compare;
pub mod runner;
pub mod scenario;
pub mod skel;
pub mod wcdb_dump;

use std::path::{Path, PathBuf};

/// Repository root: `OPENSVN_REPO_ROOT` or two levels above this crate.
pub fn repo_root() -> PathBuf {
    std::env::var_os("OPENSVN_REPO_ROOT").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
        },
        PathBuf::from,
    )
}

/// Host directory bind-mounted at `/work` in client containers.
pub fn default_work_root() -> PathBuf {
    repo_root().join("target").join("compat").join("work")
}

#[cfg(test)]
mod scenario_files {
    use super::*;
    use crate::scenario::Scenario;

    fn yaml_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                yaml_files(&path, out)?;
            } else if path.extension().is_some_and(|e| e == "yaml") {
                out.push(path);
            }
        }
        Ok(())
    }

    /// Every harness scenario (default and legacy) parses and validates without Docker, so a
    /// schema mistake is caught by the unit tests rather than by the slow compat job.
    #[test]
    fn harness_scenarios_load() -> anyhow::Result<()> {
        let mut files = Vec::new();
        yaml_files(
            &repo_root().join("tests/compat/scenarios/harness"),
            &mut files,
        )?;
        assert!(!files.is_empty());
        for path in &files {
            Scenario::load(path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod docker_tests {
    use super::*;
    use crate::runner::{DockerBackend, Runner};
    use crate::scenario::Scenario;

    /// Needs the compose environment up and fixtures loaded. Run with `-- --ignored`.
    #[test]
    #[ignore = "requires Docker: cargo xtask compat up && cargo xtask compat fixtures"]
    fn harness_scenarios_pass_svn15_vs_svn15() -> anyhow::Result<()> {
        let root = repo_root();
        let backend = DockerBackend::new(&root);
        let runner = Runner::new(&backend, default_work_root());
        let dir = root.join("tests/compat/scenarios/harness");
        let mut ran = 0;
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "yaml") {
                let report = runner.run(&Scenario::load(&path)?)?;
                assert!(
                    report.passed(),
                    "{}: {:#?}",
                    path.display(),
                    report.mismatches
                );
                ran += 1;
            }
        }
        assert!(ran > 0, "no harness scenarios found in {}", dir.display());
        Ok(())
    }
}

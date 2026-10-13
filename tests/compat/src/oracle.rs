//! Shared lookup for the *native* svn oracle used by unit-level tests in other crates
//! (file:// repositories, single version, no Docker).
//!
//! Contract (environment variables):
//! - `OPENSVN_SVN_BIN` / `OPENSVN_SVNADMIN_BIN`: binary paths; default `svn` / `svnadmin` on PATH.
//! - `OPENSVN_REQUIRE_SVN=1`: a missing or too-old binary is an error (CI sets this);
//!   otherwise [`Oracle::resolve`] returns `Ok(None)` so the test can skip.
//!
//! Typical use in another crate's test (add `opensvn-compat` as a dev-dependency):
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use opensvn_compat::oracle::{MinVersion, Oracle};
//! let Some(oracle) = Oracle::resolve(MinVersion::V1_15)? else { return Ok(()) };
//! let status = oracle.svnadmin().args(["create", "/tmp/repo"]).status()?;
//! assert!(status.success());
//! # Ok(()) }
//! ```

use std::path::PathBuf;
use std::process::Command;

/// Minimum acceptable svn version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinVersion {
    /// Subversion 1.14 or newer (ask for this only for 1.14-specific behaviour).
    V1_14,
    /// Subversion 1.15 or newer (the default expectation).
    V1_15,
}

impl MinVersion {
    fn pair(self) -> (u32, u32) {
        match self {
            Self::V1_14 => (1, 14),
            Self::V1_15 => (1, 15),
        }
    }
}

/// Why the oracle is unusable.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OracleError {
    /// Binary missing or not runnable while `OPENSVN_REQUIRE_SVN=1`.
    #[error("svn oracle `{bin}` unavailable ({reason}) and OPENSVN_REQUIRE_SVN=1")]
    Missing {
        /// Binary that was tried.
        bin: String,
        /// Why it failed.
        reason: String,
    },
    /// Binary older than required while `OPENSVN_REQUIRE_SVN=1`.
    #[error("svn oracle `{bin}` is {found}, need >= {required}")]
    TooOld {
        /// Binary that was tried.
        bin: String,
        /// Version found.
        found: String,
        /// Version required.
        required: String,
    },
}

/// A resolved native svn installation.
#[derive(Debug, Clone)]
pub struct Oracle {
    svn: PathBuf,
    svnadmin: PathBuf,
    version: (u32, u32, u32),
}

impl Oracle {
    /// Resolve from the process environment. `Ok(None)` means "skip this test" (binary
    /// missing/too old and `OPENSVN_REQUIRE_SVN` unset); `Err` means "fail this test".
    pub fn resolve(min: MinVersion) -> Result<Option<Self>, OracleError> {
        Self::resolve_with(|k| std::env::var(k).ok(), min)
    }

    /// Same as [`Oracle::resolve`] with an injectable environment (for tests).
    pub fn resolve_with(
        env: impl Fn(&str) -> Option<String>,
        min: MinVersion,
    ) -> Result<Option<Self>, OracleError> {
        let required = env("OPENSVN_REQUIRE_SVN").is_some_and(|v| v == "1");
        let svn = env("OPENSVN_SVN_BIN").unwrap_or_else(|| "svn".into());
        let svnadmin = env("OPENSVN_SVNADMIN_BIN").unwrap_or_else(|| "svnadmin".into());
        let found = match probe_version(&svn) {
            Ok(v) => v,
            Err(reason) if required => return Err(OracleError::Missing { bin: svn, reason }),
            Err(reason) => {
                eprintln!(
                    "skipping: svn oracle `{svn}` unavailable ({reason}); set OPENSVN_REQUIRE_SVN=1 to make this fatal"
                );
                return Ok(None);
            }
        };
        let (maj, minor) = min.pair();
        if (found.0, found.1) < (maj, minor) {
            let found_s = format!("{}.{}.{}", found.0, found.1, found.2);
            if required {
                return Err(OracleError::TooOld {
                    bin: svn,
                    found: found_s,
                    required: format!("{maj}.{minor}"),
                });
            }
            eprintln!("skipping: svn oracle `{svn}` is {found_s}, need >= {maj}.{minor}");
            return Ok(None);
        }
        Ok(Some(Self {
            svn: svn.into(),
            svnadmin: svnadmin.into(),
            version: found,
        }))
    }

    /// `(major, minor, patch)` of the resolved svn.
    pub fn version(&self) -> (u32, u32, u32) {
        self.version
    }

    /// A `Command` for `svn`.
    pub fn svn(&self) -> Command {
        Command::new(&self.svn)
    }

    /// A `Command` for `svnadmin`.
    pub fn svnadmin(&self) -> Command {
        Command::new(&self.svnadmin)
    }
}

fn probe_version(bin: &str) -> Result<(u32, u32, u32), String> {
    let out = Command::new(bin)
        .args(["--version", "--quiet"])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("exit {}", out.status));
    }
    parse_version(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| {
        format!(
            "unparseable version {:?}",
            String::from_utf8_lossy(&out.stdout).trim()
        )
    })
}

/// Parse `1.15.0` (optionally with a suffix such as `-dev`) into a triple.
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.trim().split('.');
    let num = |p: &str| -> Option<u32> {
        p.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    };
    Some((
        num(parts.next()?)?,
        num(parts.next()?)?,
        parts.next().and_then(num).unwrap_or(0),
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    fn stub(dir: &std::path::Path, version: &str) -> std::io::Result<String> {
        let path = dir.join("svn-stub");
        std::fs::write(&path, format!("#!/bin/sh\necho {version}\n"))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        Ok(path.to_string_lossy().into_owned())
    }

    fn env(pairs: &[(&str, String)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("1.15.0\n"), Some((1, 15, 0)));
        assert_eq!(parse_version("1.14.5-dev"), Some((1, 14, 5)));
        assert_eq!(parse_version("nonsense"), None);
    }

    #[test]
    fn resolves_acceptable_binary() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let bin = stub(tmp.path(), "1.15.0")?;
        let o = Oracle::resolve_with(env(&[("OPENSVN_SVN_BIN", bin)]), MinVersion::V1_15)?;
        assert_eq!(o.map(|o| o.version()), Some((1, 15, 0)));
        Ok(())
    }

    #[test]
    fn too_old_skips_unless_required() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let bin = stub(tmp.path(), "1.14.5")?;
        let base = [("OPENSVN_SVN_BIN", bin)];
        assert!(Oracle::resolve_with(env(&base), MinVersion::V1_15)?.is_none());
        assert!(Oracle::resolve_with(env(&base), MinVersion::V1_14)?.is_some());
        let mut required = base.to_vec();
        required.push(("OPENSVN_REQUIRE_SVN", "1".into()));
        assert!(matches!(
            Oracle::resolve_with(env(&required), MinVersion::V1_15),
            Err(OracleError::TooOld { .. })
        ));
        Ok(())
    }

    #[test]
    fn missing_binary_skips_unless_required() -> anyhow::Result<()> {
        let bad = ("OPENSVN_SVN_BIN", "/nonexistent/svn".to_string());
        assert!(
            Oracle::resolve_with(env(std::slice::from_ref(&bad)), MinVersion::V1_15)?.is_none()
        );
        let required = [bad, ("OPENSVN_REQUIRE_SVN", "1".into())];
        assert!(matches!(
            Oracle::resolve_with(env(&required), MinVersion::V1_15),
            Err(OracleError::Missing { .. })
        ));
        Ok(())
    }
}

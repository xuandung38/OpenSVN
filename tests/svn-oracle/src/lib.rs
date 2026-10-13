//! Native svn oracle for unit-level tests (file:// repositories, one svn version, no Docker).
//!
//! Standalone on purpose: it depends on no engine crate, so any crate can use it as a
//! dev-dependency without dependency cycles.
//!
//! Environment contract:
//! - `OPENSVN_SVN_BIN`: path to `svn` (default `svn` on PATH).
//! - `OPENSVN_SVNADMIN_BIN`: path to `svnadmin`. When unset, `svnadmin` is taken from the same
//!   directory as `OPENSVN_SVN_BIN` (so both come from one installation), or from PATH if
//!   `OPENSVN_SVN_BIN` has no directory part.
//! - `OPENSVN_REQUIRE_SVN`: `1|true|yes|on` (case-insensitive) makes a missing or too-old binary an
//!   error; `0|false|no|off` or unset lets [`Oracle::resolve`] return `Ok(None)` so the test skips.
//!   Any other value is an error, never a silent skip.
//!
//! Both binaries' versions are checked. Commands from [`Oracle::svn`] carry an isolated
//! `--config-dir` (a temp dir owned by the [`Oracle`]) and `--non-interactive`; all commands
//! get a UTF-8 `LC_ALL`. Version probes are cached per process.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use opensvn_svn_oracle::{MinVersion, Oracle};
//! let Some(oracle) = Oracle::resolve(MinVersion::V1_15)? else { return Ok(()) };
//! assert!(oracle.svnadmin().args(["create", "/tmp/repo"]).status()?.success());
//! assert!(oracle.svn().args(["info", "file:///tmp/repo"]).status()?.success());
//! # Ok(()) }
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use tempfile::TempDir;

/// Minimum acceptable version for both `svn` and `svnadmin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinVersion {
    /// 1.14 or newer; ask for this only for 1.14-specific behaviour.
    V1_14,
    /// 1.15 or newer; the default expectation.
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

/// Why the oracle cannot be used.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OracleError {
    /// Binary missing or not runnable, and `OPENSVN_REQUIRE_SVN` is on.
    #[error("svn oracle `{bin}` unavailable ({reason}) and OPENSVN_REQUIRE_SVN is on")]
    Missing {
        /// Binary that was tried.
        bin: String,
        /// Why it failed.
        reason: String,
    },
    /// Binary older than required, and `OPENSVN_REQUIRE_SVN` is on.
    #[error("svn oracle `{bin}` is {found}, need >= {required}")]
    TooOld {
        /// Binary that was tried.
        bin: String,
        /// Version found.
        found: String,
        /// Version required.
        required: String,
    },
    /// `OPENSVN_REQUIRE_SVN` has a value that is not a recognised boolean.
    #[error("OPENSVN_REQUIRE_SVN={0:?} is not one of 1,true,yes,on,0,false,no,off")]
    InvalidRequireValue(String),
    /// The isolated config directory could not be created.
    #[error("cannot create isolated svn config dir: {0}")]
    ConfigDir(String),
}

/// A resolved native svn installation. Cheap to clone; clones share the config directory.
#[derive(Debug, Clone)]
pub struct Oracle {
    svn: PathBuf,
    svnadmin: PathBuf,
    version: (u32, u32, u32),
    config_dir: Arc<TempDir>,
}

impl Oracle {
    /// Resolve from the process environment. `Ok(None)` means "skip this test"; `Err` means
    /// "fail this test".
    pub fn resolve(min: MinVersion) -> Result<Option<Self>, OracleError> {
        Self::resolve_with(|k| std::env::var(k).ok(), min)
    }

    /// Same as [`Oracle::resolve`] with an injectable environment (for tests).
    pub fn resolve_with(
        env: impl Fn(&str) -> Option<String>,
        min: MinVersion,
    ) -> Result<Option<Self>, OracleError> {
        let required = parse_require(env("OPENSVN_REQUIRE_SVN").as_deref())?;
        let svn = env("OPENSVN_SVN_BIN").unwrap_or_else(|| "svn".into());
        let svnadmin = env("OPENSVN_SVNADMIN_BIN").unwrap_or_else(|| sibling_svnadmin(&svn));

        let mut versions = Vec::new();
        for bin in [&svn, &svnadmin] {
            match check_binary(bin, min) {
                Ok(v) => versions.push(v),
                Err(err) if required => return Err(err),
                Err(err) => {
                    eprintln!("skipping: {err}; set OPENSVN_REQUIRE_SVN=1 to make this fatal");
                    return Ok(None);
                }
            }
        }
        let config_dir = tempfile::Builder::new()
            .prefix("opensvn-svn-config-")
            .tempdir()
            .map_err(|e| OracleError::ConfigDir(e.to_string()))?;
        Ok(Some(Self {
            svn: svn.into(),
            svnadmin: svnadmin.into(),
            version: versions[0],
            config_dir: Arc::new(config_dir),
        }))
    }

    /// `(major, minor, patch)` of `svn`.
    pub fn version(&self) -> (u32, u32, u32) {
        self.version
    }

    /// `svn` with `--config-dir <isolated> --non-interactive` already applied.
    pub fn svn(&self) -> Command {
        let mut cmd = Command::new(&self.svn);
        cmd.arg("--config-dir")
            .arg(self.config_dir.path())
            .arg("--non-interactive");
        apply_locale(&mut cmd);
        cmd
    }

    /// `svnadmin`. It accepts neither `--config-dir` nor `--non-interactive` on every
    /// subcommand, so only the locale is set.
    pub fn svnadmin(&self) -> Command {
        let mut cmd = Command::new(&self.svnadmin);
        apply_locale(&mut cmd);
        cmd
    }

    /// The isolated svn configuration directory.
    pub fn config_dir(&self) -> &Path {
        self.config_dir.path()
    }
}

/// UTF-8 locale for deterministic output and path handling; macOS lacks `C.UTF-8`.
fn apply_locale(cmd: &mut Command) {
    let locale = if cfg!(target_os = "macos") {
        "en_US.UTF-8"
    } else {
        "C.UTF-8"
    };
    cmd.env("LC_ALL", locale);
}

fn parse_require(value: Option<&str>) -> Result<bool, OracleError> {
    match value.map(str::to_ascii_lowercase).as_deref() {
        None | Some("0" | "false" | "no" | "off") => Ok(false),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some(_) => Err(OracleError::InvalidRequireValue(
            value.unwrap_or_default().to_string(),
        )),
    }
}

/// `svnadmin` next to `svn` when `svn` is a path, else plain `svnadmin` from PATH.
fn sibling_svnadmin(svn: &str) -> String {
    let path = Path::new(svn);
    match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => {
            let name = if cfg!(windows) {
                "svnadmin.exe"
            } else {
                "svnadmin"
            };
            dir.join(name).to_string_lossy().into_owned()
        }
        None => "svnadmin".into(),
    }
}

fn check_binary(bin: &str, min: MinVersion) -> Result<(u32, u32, u32), OracleError> {
    let found = probe_version(bin).map_err(|reason| OracleError::Missing {
        bin: bin.to_string(),
        reason,
    })?;
    let (major, minor) = min.pair();
    if (found.0, found.1) < (major, minor) {
        return Err(OracleError::TooOld {
            bin: bin.to_string(),
            found: format!("{}.{}.{}", found.0, found.1, found.2),
            required: format!("{major}.{minor}"),
        });
    }
    Ok(found)
}

type VersionResult = Result<(u32, u32, u32), String>;

/// `<bin> --version --quiet`, cached for the life of the process.
fn probe_version(bin: &str) -> VersionResult {
    static CACHE: OnceLock<Mutex<HashMap<String, VersionResult>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock()
        && let Some(hit) = map.get(bin)
    {
        return hit.clone();
    }
    let result = run_version(bin);
    if let Ok(mut map) = cache.lock() {
        map.insert(bin.to_string(), result.clone());
    }
    result
}

fn run_version(bin: &str) -> VersionResult {
    let out = Command::new(bin)
        .args(["--version", "--quiet"])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("exit {}", out.status));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_version(&text).ok_or_else(|| format!("unparseable version {:?}", text.trim()))
}

/// Parse `1.15.0` (a suffix such as `-dev` is ignored) into a triple.
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
    use std::os::unix::fs::PermissionsExt;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn stub(dir: &Path, name: &str, version: &str) -> std::io::Result<PathBuf> {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\necho {version}\n"))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        Ok(path)
    }

    fn env(pairs: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<&str, String> = pairs.into_iter().collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn parses_versions_and_require_values() {
        assert_eq!(parse_version("1.15.0\n"), Some((1, 15, 0)));
        assert_eq!(parse_version("1.14.5-dev"), Some((1, 14, 5)));
        assert_eq!(parse_version("nonsense"), None);
        for yes in ["1", "true", "YES", "On"] {
            assert_eq!(parse_require(Some(yes)), Ok(true), "{yes}");
        }
        for no in ["0", "False", "no", "OFF"] {
            assert_eq!(parse_require(Some(no)), Ok(false), "{no}");
        }
        assert_eq!(parse_require(None), Ok(false));
        assert!(matches!(
            parse_require(Some("maybe")),
            Err(OracleError::InvalidRequireValue(_))
        ));
        assert!(matches!(
            parse_require(Some("")),
            Err(OracleError::InvalidRequireValue(_))
        ));
    }

    #[test]
    fn invalid_require_value_is_an_error_even_with_working_binaries() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let svn = stub(tmp.path(), "svn", "1.15.0")?;
        stub(tmp.path(), "svnadmin", "1.15.0")?;
        let e = env(vec![
            ("OPENSVN_SVN_BIN", svn.to_string_lossy().into_owned()),
            ("OPENSVN_REQUIRE_SVN", "sure".into()),
        ]);
        assert!(matches!(
            Oracle::resolve_with(e, MinVersion::V1_15),
            Err(OracleError::InvalidRequireValue(_))
        ));
        Ok(())
    }

    #[test]
    fn svnadmin_is_taken_from_the_svn_directory_and_its_version_is_checked() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let svn = stub(tmp.path(), "svn", "1.15.0")?;
        let admin = stub(tmp.path(), "svnadmin", "1.14.5")?;
        let base = vec![("OPENSVN_SVN_BIN", svn.to_string_lossy().into_owned())];

        // svnadmin 1.14.5 next to svn 1.15.0 is too old for a 1.15 requirement.
        assert!(Oracle::resolve_with(env(base.clone()), MinVersion::V1_15)?.is_none());
        let mut required = base.clone();
        required.push(("OPENSVN_REQUIRE_SVN", "TRUE".into()));
        let err = Oracle::resolve_with(env(required), MinVersion::V1_15).err();
        assert!(
            matches!(&err, Some(OracleError::TooOld { bin, .. }) if *bin == admin.to_string_lossy()),
            "expected TooOld for svnadmin, got {err:?}"
        );
        // An explicit override wins over the sibling lookup.
        let good = stub(tmp.path(), "admin-new", "1.15.1")?;
        let mut over = base;
        over.push(("OPENSVN_SVNADMIN_BIN", good.to_string_lossy().into_owned()));
        let oracle = Oracle::resolve_with(env(over), MinVersion::V1_15)?;
        assert_eq!(oracle.map(|o| o.version()), Some((1, 15, 0)));
        Ok(())
    }

    #[test]
    fn missing_binary_skips_unless_required() -> TestResult {
        let bad = ("OPENSVN_SVN_BIN", "/nonexistent/svn".to_string());
        assert!(Oracle::resolve_with(env(vec![bad.clone()]), MinVersion::V1_15)?.is_none());
        let required = env(vec![bad, ("OPENSVN_REQUIRE_SVN", "on".into())]);
        assert!(matches!(
            Oracle::resolve_with(required, MinVersion::V1_15),
            Err(OracleError::Missing { .. })
        ));
        Ok(())
    }

    #[test]
    fn commands_are_isolated_and_locale_pinned() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let svn = stub(tmp.path(), "svn", "1.15.0")?;
        stub(tmp.path(), "svnadmin", "1.15.0")?;
        let e = env(vec![(
            "OPENSVN_SVN_BIN",
            svn.to_string_lossy().into_owned(),
        )]);
        let oracle = Oracle::resolve_with(e, MinVersion::V1_15)?.ok_or("expected oracle")?;

        let cmd = oracle.svn();
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "--config-dir");
        assert_eq!(Path::new(&args[1]), oracle.config_dir());
        assert_eq!(args[2], "--non-interactive");
        let locale = cmd
            .get_envs()
            .find(|(k, _)| *k == "LC_ALL")
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().into_owned());
        assert!(
            locale.as_deref().is_some_and(|l| l.ends_with(".UTF-8")),
            "{locale:?}"
        );
        assert_eq!(oracle.svnadmin().get_args().count(), 0);
        // Separate resolutions never share a config directory.
        let again = Oracle::resolve_with(
            env(vec![(
                "OPENSVN_SVN_BIN",
                svn.to_string_lossy().into_owned(),
            )]),
            MinVersion::V1_15,
        )?
        .ok_or("expected oracle")?;
        assert_ne!(again.config_dir(), oracle.config_dir());
        Ok(())
    }

    /// Runs against whatever svn the machine has; skips when none is installed.
    #[test]
    fn real_svn_creates_a_repo_and_reads_it_back() -> TestResult {
        let Some(oracle) = Oracle::resolve(MinVersion::V1_14)? else {
            return Ok(());
        };
        let tmp = tempfile::tempdir()?;
        let repo = tmp.path().join("repo");
        assert!(
            oracle
                .svnadmin()
                .arg("create")
                .arg(&repo)
                .status()?
                .success()
        );
        let url = format!("file://{}", repo.display());
        let out = oracle
            .svn()
            .args(["info", "--show-item", "revision", &url])
            .output()?;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0");
        Ok(())
    }
}

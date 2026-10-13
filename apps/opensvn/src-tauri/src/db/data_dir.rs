//! Per-OS application data directory (docs/system-design/02 section 2.4).
//!
//! The folder is named after the Tauri bundle identifier (`tauri.conf.json`, currently
//! `com.opensvn.desktop`), like Tauri's own `app_data_dir`. The resolution is a pure function of the
//! OS, the identifier and an environment lookup so every OS layout can be tested on any host.

use std::path::PathBuf;

/// Environment variable that overrides the data directory (development and tests).
pub const DATA_DIR_ENV: &str = "OPENSVN_DATA_DIR";

/// Operating system families with distinct data directory conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    /// Windows: `%APPDATA%\<identifier>`.
    Windows,
    /// macOS: `~/Library/Application Support/<identifier>`.
    Darwin,
    /// Linux and other Unix: `$XDG_DATA_HOME/<identifier>` or `~/.local/share/<identifier>`.
    Linux,
}

impl Os {
    /// The OS this binary was compiled for.
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Darwin
        } else {
            Os::Linux
        }
    }
}

/// Data directory resolution failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DataDirError {
    /// No home or application data location could be determined.
    #[error("cannot determine the data directory: {0} is not set")]
    Unresolved(&'static str),
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Resolves the data directory for `os` using `env` for lookups.
pub fn resolve_data_dir(
    os: Os,
    identifier: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<PathBuf, DataDirError> {
    if let Some(dir) = non_empty(env(DATA_DIR_ENV)) {
        return Ok(PathBuf::from(dir));
    }
    match os {
        Os::Windows => non_empty(env("APPDATA"))
            .map(|d| PathBuf::from(d).join(identifier))
            .ok_or(DataDirError::Unresolved("APPDATA")),
        Os::Darwin => non_empty(env("HOME"))
            .map(|h| {
                PathBuf::from(h)
                    .join("Library")
                    .join("Application Support")
                    .join(identifier)
            })
            .ok_or(DataDirError::Unresolved("HOME")),
        Os::Linux => {
            if let Some(x) = non_empty(env("XDG_DATA_HOME")) {
                // The XDG spec ignores relative values.
                let p = PathBuf::from(x);
                if p.is_absolute() {
                    return Ok(p.join(identifier));
                }
            }
            non_empty(env("HOME"))
                .map(|h| {
                    PathBuf::from(h)
                        .join(".local")
                        .join("share")
                        .join(identifier)
                })
                .ok_or(DataDirError::Unresolved("HOME"))
        }
    }
}

/// Resolves the data directory of the running process from the real environment.
pub fn current_data_dir(identifier: &str) -> Result<PathBuf, DataDirError> {
    resolve_data_dir(Os::current(), identifier, &|k| std::env::var(k).ok())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const ID: &str = "com.opensvn.desktop";

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn per_os_layout() {
        let e = env(&[
            ("HOME", "/home/u"),
            ("APPDATA", "C:\\Users\\u\\AppData\\Roaming"),
        ]);
        assert_eq!(
            resolve_data_dir(Os::Darwin, ID, &e).unwrap(),
            PathBuf::from("/home/u/Library/Application Support/com.opensvn.desktop")
        );
        assert_eq!(
            resolve_data_dir(Os::Linux, ID, &e).unwrap(),
            PathBuf::from("/home/u/.local/share/com.opensvn.desktop")
        );
        assert_eq!(
            resolve_data_dir(Os::Windows, ID, &e).unwrap(),
            PathBuf::from("C:\\Users\\u\\AppData\\Roaming").join("com.opensvn.desktop")
        );
    }

    #[test]
    fn xdg_data_home_wins_when_absolute() {
        let e = env(&[("HOME", "/home/u"), ("XDG_DATA_HOME", "/data")]);
        assert_eq!(
            resolve_data_dir(Os::Linux, ID, &e).unwrap(),
            PathBuf::from("/data/com.opensvn.desktop")
        );
        let rel = env(&[("HOME", "/home/u"), ("XDG_DATA_HOME", "relative")]);
        assert_eq!(
            resolve_data_dir(Os::Linux, ID, &rel).unwrap(),
            PathBuf::from("/home/u/.local/share/com.opensvn.desktop")
        );
    }

    #[test]
    fn override_and_missing_variables() {
        let e = env(&[(DATA_DIR_ENV, "/tmp/x")]);
        assert_eq!(
            resolve_data_dir(Os::Windows, ID, &e).unwrap(),
            PathBuf::from("/tmp/x")
        );
        assert_eq!(
            resolve_data_dir(Os::Darwin, ID, &env(&[])),
            Err(DataDirError::Unresolved("HOME"))
        );
        assert_eq!(
            resolve_data_dir(Os::Windows, ID, &env(&[("APPDATA", " ")])),
            Err(DataDirError::Unresolved("APPDATA"))
        );
    }
}

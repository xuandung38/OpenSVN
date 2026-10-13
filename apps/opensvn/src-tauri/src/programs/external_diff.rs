//! Opening a diff in the external diff tool (`diff.externalTool`).
//!
//! The two sides are written as read-only copies into a fresh folder under the private
//! `external-diff` directory and the configured program is started directly (never through a
//! shell) with `{left}` / `{right}` in its arguments replaced by those copies. When the arguments
//! name neither placeholder, the two paths are appended, which is what most tools expect.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use opensvn_ipc::AppError;

use super::is_deceptive;

/// Placeholder replaced by the left (older) copy.
pub const LEFT_TOKEN: &str = "{left}";
/// Placeholder replaced by the right (newer) copy.
pub const RIGHT_TOKEN: &str = "{right}";
/// Staged copies older than this are removed on the next launch. External tools may outlive the
/// command (some return at once and keep the window open), so copies are never removed sooner.
pub const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Starts a program. Tests substitute a recorder; the app uses [`NativeLauncher`].
pub trait ProgramLauncher: Send + Sync {
    /// Starts `program` with `args` and returns without waiting for it to exit.
    fn launch(&self, program: &Path, args: &[String]) -> Result<(), AppError>;
}

/// Launches through the operating system, without a shell.
pub struct NativeLauncher;

impl ProgramLauncher for NativeLauncher {
    fn launch(&self, program: &Path, args: &[String]) -> Result<(), AppError> {
        use std::process::{Command, Stdio};
        let is_mac_bundle = cfg!(target_os = "macos")
            && program
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("app"));
        let mut cmd = if is_mac_bundle {
            let mut c = Command::new("/usr/bin/open");
            c.arg("-n").arg("-a").arg(program).arg("--args");
            c
        } else {
            Command::new(program)
        };
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().map_err(|e| {
            crate::errors::invalid_argument("The external diff tool could not be started.")
                .with_detail(e.to_string())
        })?;
        // Reap the process when it exits so it never lingers as a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

/// Arguments for the tool: placeholders replaced, or both paths appended when there are none.
pub fn expand_args(args: &[String], left: &str, right: &str) -> Vec<String> {
    let uses_tokens = args
        .iter()
        .any(|a| a.contains(LEFT_TOKEN) || a.contains(RIGHT_TOKEN));
    if !uses_tokens {
        let mut out = args.to_vec();
        out.push(left.to_owned());
        out.push(right.to_owned());
        return out;
    }
    args.iter()
        .map(|a| a.replace(LEFT_TOKEN, left).replace(RIGHT_TOKEN, right))
        .collect()
}

/// File name for the staged copies: the last segment of `path`, reduced to characters that are
/// safe in a file name on every OS (the extension is kept so tools pick the right syntax).
pub fn staged_file_name(path: &str) -> String {
    let last = path
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let cleaned: String = last
        .chars()
        .map(|c| {
            if is_deceptive(c) || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .take(120)
        .collect();
    let trimmed = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        return "file".to_owned();
    }
    // Device names stay special on Windows whatever the extension (`NUL.txt`).
    let stem = trimmed.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        format!("_{trimmed}")
    } else {
        trimmed.to_owned()
    }
}

fn io_error(what: &str, e: &std::io::Error) -> AppError {
    AppError::internal(format!(
        "Could not prepare the files for the external diff tool ({what})."
    ))
    .with_detail(e.to_string())
}

fn write_copy(dir: &Path, name: &str, data: &[u8]) -> Result<PathBuf, AppError> {
    std::fs::create_dir(dir).map_err(|e| io_error("folder", &e))?;
    let path = dir.join(name);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| io_error("file", &e))?;
    f.write_all(data).map_err(|e| io_error("write", &e))?;
    drop(f);
    // Read-only: edits in the tool must not look like they reach the working copy.
    let mut perms = std::fs::metadata(&path)
        .map_err(|e| io_error("metadata", &e))?
        .permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms).map_err(|e| io_error("permissions", &e))?;
    Ok(path)
}

/// Writes both sides under a new folder in `root` (the private external-diff directory) and
/// returns their paths. A missing side (added or deleted file) is staged as an empty file.
pub fn stage_sides(
    root: &Path,
    name: &str,
    left: &[u8],
    right: &[u8],
) -> Result<(PathBuf, PathBuf), AppError> {
    let session = root.join(uuid::Uuid::now_v7().simple().to_string());
    std::fs::create_dir(&session).map_err(|e| io_error("folder", &e))?;
    let l = write_copy(&session.join("left"), name, left)?;
    let r = write_copy(&session.join("right"), name, right)?;
    Ok((l, r))
}

/// Best effort: removes staged folders in `root` last modified more than `max_age` ago.
pub fn sweep_stale(root: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let old = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > max_age);
        if meta.is_dir() && old {
            remove_tree(&entry.path());
        }
    }
}

/// Removes a staged folder; the copies are read-only, which blocks removal on Windows.
fn remove_tree(dir: &Path) {
    for side in ["left", "right"] {
        if let Ok(files) = std::fs::read_dir(dir.join(side)) {
            for f in files.flatten() {
                if let Ok(meta) = std::fs::symlink_metadata(f.path()) {
                    let mut perms = meta.permissions();
                    #[allow(clippy::permissions_set_readonly_false)]
                    perms.set_readonly(false);
                    let _ = std::fs::set_permissions(f.path(), perms);
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn placeholders_are_replaced_inside_arguments() {
        assert_eq!(
            expand_args(&s(&["--diff", "--left={left}", "{right}"]), "/a", "/b"),
            s(&["--diff", "--left=/a", "/b"])
        );
    }

    #[test]
    fn without_placeholders_both_paths_are_appended() {
        assert_eq!(expand_args(&s(&["-w"]), "/a", "/b"), s(&["-w", "/a", "/b"]));
        assert_eq!(expand_args(&[], "/a", "/b"), s(&["/a", "/b"]));
    }

    #[test]
    fn staged_names_keep_the_extension_and_drop_unsafe_characters() {
        assert_eq!(staged_file_name("src/engine/renderer.ts"), "renderer.ts");
        assert_eq!(staged_file_name("a/b/"), "b");
        assert_eq!(staged_file_name("x/..\u{202E}gpj.exe"), "_gpj.exe");
        assert_eq!(staged_file_name("we:ird|na*me?.txt"), "we_ird_na_me_.txt");
        assert_eq!(staged_file_name(""), "file");
        assert_eq!(staged_file_name("/.."), "file");
        assert_eq!(staged_file_name("dir/nul.txt"), "_nul.txt");
        assert_eq!(staged_file_name("COM1"), "_COM1");
    }

    #[test]
    fn sides_are_staged_read_only_in_separate_folders() {
        let root = tempfile::tempdir().unwrap();
        let (l, r) = stage_sides(root.path(), "a.txt", b"old\n", b"").unwrap();
        assert_eq!(std::fs::read(&l).unwrap(), b"old\n");
        assert_eq!(std::fs::read(&r).unwrap(), b"");
        assert_ne!(l.parent(), r.parent());
        assert!(l.starts_with(root.path()) && r.starts_with(root.path()));
        assert!(std::fs::metadata(&l).unwrap().permissions().readonly());
        // A second launch never reuses or overwrites the first copies.
        let (l2, _) = stage_sides(root.path(), "a.txt", b"new\n", b"").unwrap();
        assert_ne!(l, l2);
        assert_eq!(std::fs::read(&l).unwrap(), b"old\n");
    }

    #[test]
    fn sweep_removes_only_stale_folders() {
        let root = tempfile::tempdir().unwrap();
        let (l, _) = stage_sides(root.path(), "a.txt", b"x", b"y").unwrap();
        sweep_stale(root.path(), STALE_AFTER);
        assert!(l.exists());
        sweep_stale(root.path(), Duration::ZERO);
        std::thread::sleep(Duration::from_millis(20));
        sweep_stale(root.path(), Duration::ZERO);
        assert!(!l.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

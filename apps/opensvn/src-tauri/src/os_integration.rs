//! Reveal in the file manager, open with the default app, editor or terminal.
//!
//! Every path is a path *inside a registered working copy*: it is built from a validated relative
//! path with `safe_join` and must exist. "Default app" never opens programs or scripts, because a
//! repository can contain files that run when opened. Editor and terminal are the programs the
//! user picked in a native dialog (`program_choose`).

use std::path::{Path, PathBuf};
use std::process::Command;

use opensvn_ipc::{AppError, ErrorAction, ErrorCode, ExternalDiffTool, OpenWith};
use svn_types::{RelPath, safe_join};

use crate::errors::invalid_argument;

/// File extensions that run code when opened (or install/mount software). Compared lower-case.
const RUNNABLE_EXTENSIONS: &[&str] = &[
    "exe", "com", "bat", "cmd", "msi", "msp", "scr", "pif", "lnk", "url", "reg", "ps1", "psm1",
    "vbs", "vbe", "js", "jse", "wsf", "wsh", "hta", "cpl", "jar", "sh", "bash", "zsh", "csh",
    "command", "app", "pkg", "dmg", "workflow", "desktop", "appimage", "run", "bin", "deb", "rpm",
    "py", "pl", "rb", "scpt", "action",
];

/// The side effects of this module, behind a trait so tests can record them.
pub trait OsOpener: Send + Sync {
    /// Shows `path` selected in the file manager.
    fn reveal(&self, path: &Path) -> Result<(), AppError>;
    /// Opens `path` with the operating system's default application.
    fn open_default(&self, path: &Path) -> Result<(), AppError>;
    /// Runs `program args...` (no shell) in `cwd`.
    fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<(), AppError>;
}

/// Whether the default application must not be used for `path`: files that run code, and (on
/// Unix) anything with an execute bit.
pub fn is_runnable(path: &Path) -> bool {
    let by_name = path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        RUNNABLE_EXTENSIONS
            .iter()
            .any(|r| r.eq_ignore_ascii_case(e))
    });
    if by_name {
        return true;
    }
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(path) {
        use std::os::unix::fs::PermissionsExt;
        return meta.is_file() && meta.permissions().mode() & 0o111 != 0;
    }
    false
}

/// Resolves a working copy relative path to an existing path below `root`.
pub fn resolve_in_wc(root: &Path, rel: &str) -> Result<PathBuf, AppError> {
    let rel = RelPath::new(rel).map_err(|e| AppError::from(&e))?;
    let full = safe_join(root, &rel).map_err(|e| AppError::from(&e))?;
    if !full.exists() {
        let mut e = AppError::internal("This item is not on disk.").with_path(rel.as_str());
        e.code = ErrorCode::PathNotFound;
        return Err(e);
    }
    Ok(full)
}

fn not_configured(what: &str) -> AppError {
    let mut e = invalid_argument(format!("No {what} is configured. Choose one in Settings."));
    e.actions = Some(vec![ErrorAction::OpenSettings]);
    e
}

/// Reveals `rel` of the working copy at `root`.
pub fn reveal_in_os(opener: &dyn OsOpener, root: &Path, rel: &str) -> Result<(), AppError> {
    opener.reveal(&resolve_in_wc(root, rel)?)
}

/// Opens `rel` with the default app, the configured editor or the configured terminal.
pub fn open_external(
    opener: &dyn OsOpener,
    root: &Path,
    rel: &str,
    with: OpenWith,
    editor: Option<&ExternalDiffTool>,
    terminal: Option<&ExternalDiffTool>,
) -> Result<(), AppError> {
    let path = resolve_in_wc(root, rel)?;
    match with {
        OpenWith::Default => {
            if is_runnable(&path) {
                return Err(invalid_argument(
                    "This kind of file can run code, so it is not opened automatically. Open it from your file manager if you trust it.",
                )
                .with_path(rel));
            }
            opener.open_default(&path)
        }
        OpenWith::Editor => {
            let tool = editor.ok_or_else(|| not_configured("editor"))?;
            let mut args = tool.args.clone();
            args.push(path.to_string_lossy().into_owned());
            opener.run(&tool.path, &args, path.parent().unwrap_or(root))
        }
        OpenWith::Terminal => {
            let tool = terminal.ok_or_else(|| not_configured("terminal"))?;
            let dir = if path.is_dir() {
                path.clone()
            } else {
                path.parent().unwrap_or(root).to_path_buf()
            };
            opener.run(&tool.path, &tool.args, &dir)
        }
    }
}

/// [`OsOpener`] using the platform's launcher commands (no shell is involved).
pub struct SystemOpener;

fn launch(mut cmd: Command) -> Result<(), AppError> {
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| AppError::internal("Could not start the program.").with_detail(e.to_string()))
}

impl OsOpener for SystemOpener {
    fn reveal(&self, path: &Path) -> Result<(), AppError> {
        if cfg!(target_os = "macos") {
            let mut c = Command::new("open");
            c.arg("-R").arg(path);
            launch(c)
        } else if cfg!(windows) {
            // Unverified on Windows: no runner available.
            let mut c = Command::new("explorer");
            c.arg(format!("/select,{}", path.display()));
            launch(c)
        } else {
            let target = if path.is_dir() {
                path
            } else {
                path.parent().unwrap_or(path)
            };
            let mut c = Command::new("xdg-open");
            c.arg(target);
            launch(c)
        }
    }

    fn open_default(&self, path: &Path) -> Result<(), AppError> {
        let mut c = if cfg!(target_os = "macos") {
            Command::new("open")
        } else if cfg!(windows) {
            Command::new("explorer")
        } else {
            Command::new("xdg-open")
        };
        c.arg(path);
        launch(c)
    }

    fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<(), AppError> {
        let mut c = if cfg!(target_os = "macos") && program.ends_with(".app") {
            // An application bundle is started through `open`.
            let mut c = Command::new("open");
            c.arg("-a").arg(program);
            if !args.is_empty() {
                c.arg("--args").args(args);
            }
            c
        } else {
            let mut c = Command::new(program);
            c.args(args);
            c
        };
        c.current_dir(cwd);
        launch(c)
    }
}

#[cfg(test)]
mod tests;

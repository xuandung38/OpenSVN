//! Reveal in the file manager, open with the default app, editor or terminal.
//!
//! Every path is a path *inside a registered working copy*: it is built from a validated relative
//! path (server-name rules: no trailing dot or space) with `safe_join` and must exist. "Default
//! app" only opens allow-listed document types and plain folders, never symlinks, because a
//! repository can contain files that run when opened. Editor and terminal are the programs the
//! user picked in a native dialog (`program_choose`).

use std::path::{Path, PathBuf};
use std::process::Command;

use opensvn_ipc::{AppError, ErrorAction, ErrorCode, ExternalDiffTool, OpenWith};
use svn_types::{RelPath, safe_join};

use crate::errors::invalid_argument;

/// File extensions the default application may open: plain documents, images and office files
/// without macros. Compared case-insensitively. Everything else (programs, scripts, installers,
/// shortcuts, disk images, archives, web pages, macro-enabled office files, unknown types) is
/// refused and the UI offers "Reveal in file manager" instead.
const DEFAULT_APP_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "rst", "log", "csv", "tsv", "json", "yaml", "yml", "toml", "diff",
    "patch", "png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "pdf", "rtf", "doc",
    "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp",
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

/// Whether the default application may open the file at `path`: its name has an allow-listed
/// extension and (on Unix) it has no execute bit. The caller has already refused symlinks.
pub fn default_app_allowed(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // Windows strips trailing dots and spaces when it opens a file, so `a.exe.` runs `a.exe`.
    // Such names are already refused by `resolve_in_wc`; trimming here keeps this check honest
    // on its own.
    let name = name.trim_end_matches(['.', ' ']);
    let ext = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => return false,
    };
    if !DEFAULT_APP_EXTENSIONS
        .iter()
        .any(|a| a.eq_ignore_ascii_case(ext))
    {
        return false;
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return false;
        }
    }
    true
}

/// Whether the default application may open the directory at `path`. A directory whose name has
/// an extension may be a bundle (`.app`, `.pkg`, `.prefPane`, ...) that macOS launches, so only
/// plain names are opened.
fn default_app_allows_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| !n.contains('.'))
}

/// Whether any component of `rel` below `root` is a symbolic link (or cannot be inspected).
fn has_symlink_component(root: &Path, rel: &RelPath) -> bool {
    let mut cur = root.to_path_buf();
    for seg in rel.components() {
        cur.push(seg);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if !m.file_type().is_symlink() => {}
            _ => return true,
        }
    }
    false
}

/// The error for an item the default app must not open; the UI reveals it instead.
fn default_app_refused(rel: &RelPath) -> AppError {
    let mut e = invalid_argument(
        "This kind of item is not opened automatically because it could run code. It is shown in your file manager instead.",
    )
    .with_path(rel.as_str());
    e.actions = Some(vec![ErrorAction::RevealInFileManager]);
    e
}

/// Validates `rel` with the strict rules used for names from a server: no trailing dot or space,
/// no reserved device names, no stream separators. The empty string is the root.
fn parse_rel(rel: &str) -> Result<RelPath, AppError> {
    if rel.is_empty() {
        return Ok(RelPath::root());
    }
    RelPath::from_server(rel).map_err(|e| AppError::from(&e))
}

/// Resolves a working copy relative path to an existing path below `root`.
pub fn resolve_in_wc(root: &Path, rel: &str) -> Result<PathBuf, AppError> {
    let rel = parse_rel(rel)?;
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
            let rel = parse_rel(rel)?;
            // The launcher follows links, so a committed `README.txt -> Evil.app` would run the
            // target: refuse any link inside the working copy.
            if has_symlink_component(root, &rel) {
                return Err(default_app_refused(&rel));
            }
            let allowed = if path.is_dir() {
                default_app_allows_dir(&path)
            } else {
                default_app_allowed(&path)
            };
            if !allowed {
                return Err(default_app_refused(&rel));
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

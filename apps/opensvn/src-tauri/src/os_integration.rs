//! Reveal in the file manager, open with the default app, editor or terminal.
//!
//! Every path is a path *inside a registered working copy*: it is built from a validated relative
//! path (server-name rules: no trailing dot or space) with `safe_join` and must exist. "Default
//! app" only opens allow-listed document types and plain folders, never symlinks, because a
//! repository can contain files that run when opened. Editor and terminal are the programs the
//! user picked in a native dialog (`program_choose`).
//!
//! Residual risk: the checks and the launch are not atomic (TOCTOU), so a process that can write
//! to the working copy could swap the item between the check and the launch.

use std::path::{Path, PathBuf};
use std::process::Command;

use opensvn_ipc::{AppError, ErrorAction, ErrorCode, ExternalDiffTool, OpenWith};
use svn_types::{RelPath, safe_join};

use crate::errors::invalid_argument;

/// File extensions the default application may open: plain documents, images and OOXML office
/// files that cannot hold macros. Compared case-insensitively. Everything else (programs, scripts,
/// installers, shortcuts, disk images, archives, web pages, legacy binary office files and
/// OpenDocument files, which may carry macros, RTF, which may embed objects, unknown types) is
/// refused and the UI offers "Reveal in file manager" instead.
const DEFAULT_APP_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "rst", "log", "csv", "tsv", "json", "yaml", "yml", "toml", "diff",
    "patch", "png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "pdf", "docx", "xlsx",
    "pptx",
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
    if !default_app_name_allowed(name) {
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

/// Whether a file called `name` has an extension the default application may open (the name
/// check of [`default_app_allowed`], usable before the file exists).
pub fn default_app_name_allowed(name: &str) -> bool {
    // Windows strips trailing dots and spaces when it opens a file, so `a.exe.` runs `a.exe`.
    // Such names are already refused by `resolve_in_wc`; trimming here keeps this check honest
    // on its own.
    let name = name.trim_end_matches(['.', ' ']);
    let ext = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => return false,
    };
    DEFAULT_APP_EXTENSIONS
        .iter()
        .any(|a| a.eq_ignore_ascii_case(ext))
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
    e.actions = Some(Box::new(vec![ErrorAction::RevealInFileManager]));
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
    e.actions = Some(Box::new(vec![ErrorAction::OpenSettings]));
    e
}

fn not_on_disk(rel: &RelPath) -> AppError {
    let mut e = if rel.as_str().is_empty() {
        AppError::internal(
            "The working copy folder is not on disk. Locate the working copy or remove it from the list.",
        )
    } else {
        AppError::internal("This item is not on disk.").with_path(rel.as_str())
    };
    e.code = ErrorCode::PathNotFound;
    e
}

/// Resolves `rel` (relative to the working copy `root`; empty = the root itself) to the path the
/// file manager may show.
///
/// The file manager resolves symbolic links before it selects an item, and a repository can
/// ship versioned symlinks (`svn:special`) that point anywhere (`docs -> ~/.ssh`). So every
/// component below the root must be a real file or folder, never a symlink (also when it points
/// inside the working copy or nowhere), and the canonical target must stay inside the canonical
/// root. The canonical path is returned so the file manager has nothing left to resolve.
pub fn reveal_target(root: &Path, rel: &str) -> Result<PathBuf, AppError> {
    let rel = parse_rel(rel)?;
    let mut cur = root.to_path_buf();
    for seg in rel.components() {
        cur.push(seg);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(invalid_argument(
                    "This item is a symbolic link. Only real files and folders inside the working copy are shown.",
                )
                .with_path(rel.as_str()));
            }
            Ok(_) => {}
            Err(_) => return Err(not_on_disk(&rel)),
        }
    }
    let target = safe_join(root, &rel).map_err(|e| AppError::from(&e))?;
    let canon_root = dunce::canonicalize(root).map_err(|_| not_on_disk(&RelPath::root()))?;
    let canon = dunce::canonicalize(&target).map_err(|_| not_on_disk(&rel))?;
    if !canon.starts_with(&canon_root) {
        return Err(
            invalid_argument("This item is outside the working copy.").with_path(rel.as_str())
        );
    }
    Ok(canon)
}

/// Reveals `rel` of the working copy at `root` (see [`reveal_target`]: no symlinks, canonical
/// containment).
pub fn reveal_in_os(opener: &dyn OsOpener, root: &Path, rel: &str) -> Result<(), AppError> {
    opener.reveal(&reveal_target(root, rel)?)
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
    match with {
        OpenWith::Default => {
            let path = resolve_in_wc(root, rel)?;
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
        // The editor and the terminal get the same rule as the file manager (see
        // [`reveal_target`]): a versioned symlink (`notes.txt -> /etc/shadow`, `docs -> ~/.ssh`)
        // must not hand a path outside the working copy to another program.
        OpenWith::Editor => {
            let tool = editor.ok_or_else(|| not_configured("editor"))?;
            let path = reveal_target(root, rel)?;
            let mut args = tool.args.clone();
            args.push(path.to_string_lossy().into_owned());
            opener.run(&tool.path, &args, path.parent().unwrap_or(root))
        }
        OpenWith::Terminal => {
            let tool = terminal.ok_or_else(|| not_configured("terminal"))?;
            let path = reveal_target(root, rel)?;
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
        // The opener plugin selects the item natively (Finder, Explorer's
        // SHOpenFolderAndSelectItems, the freedesktop FileManager1 interface with an `xdg-open`
        // fallback), so no command line has to quote the path.
        tauri_plugin_opener::reveal_item_in_dir(path).map_err(|e| {
            AppError::internal("The file manager could not be opened.").with_detail(e.to_string())
        })
    }

    fn open_default(&self, path: &Path) -> Result<(), AppError> {
        let mut c = if cfg!(target_os = "macos") {
            Command::new("open")
        } else if cfg!(windows) {
            // TODO(windows): use ShellExecuteW (needs a direct `windows-sys` dependency with the
            // `Win32_UI_Shell` feature). `explorer` parses its argument itself and splits it on
            // commas, so a path containing `,` may open the wrong item or a folder instead.
            Command::new("explorer")
        } else {
            Command::new("xdg-open")
        };
        c.arg(path);
        crate::startup_env::scrub(&mut c);
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
        crate::startup_env::scrub(&mut c);
        launch(c)
    }
}

#[cfg(test)]
mod tests;

//! Programs the app may launch (external diff/merge tool, editor, terminal).
//!
//! These settings are the one place where a compromised WebView could turn "change a setting"
//! into "run any executable". They therefore never go through `settings_set`: the user picks the
//! program in a native file dialog and confirms the exact command line in a native message
//! dialog; only then is the setting stored.

pub mod external_diff;
mod native;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use opensvn_ipc::{AppError, ErrorAction, ExtDiffTool, ExternalDiffTool, ProgramKind, Settings};
use tokio::sync::Mutex;

use crate::settings::{MAX_EXT_TOOLS, SettingsService};

pub use external_diff::{NativeLauncher, ProgramLauncher};
pub use native::NativeProgramPicker;

/// Native dialogs used to choose and confirm a program.
#[async_trait]
pub trait ProgramPicker: Send + Sync {
    /// Shows a file picker; `None` when the user cancels.
    async fn pick_program(&self, kind: ProgramKind) -> Option<PathBuf>;
    /// Asks the user to confirm running `command_line`; false when declined.
    async fn confirm(&self, kind: ProgramKind, command_line: &str) -> bool;
}

const MAX_ARGS: usize = 32;
const MAX_ARG_LEN: usize = 1024;
const MAX_TOTAL_ARGS: usize = 2048;

fn refuse(msg: impl Into<String>) -> AppError {
    crate::errors::invalid_argument(msg)
}

/// Checks that `path` is something the OS can launch and returns its canonical form.
fn validate_program(path: &Path) -> Result<PathBuf, AppError> {
    let canonical =
        std::fs::canonicalize(path).map_err(|_| refuse("The selected program does not exist."))?;
    let meta = std::fs::metadata(&canonical)
        .map_err(|_| refuse("The selected program cannot be read."))?;
    let is_mac_bundle = cfg!(target_os = "macos")
        && meta.is_dir()
        && canonical
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("app"));
    if !meta.is_file() && !is_mac_bundle {
        return Err(refuse("Select an executable file."));
    }
    #[cfg(unix)]
    if meta.is_file() {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(refuse("The selected file is not executable."));
        }
    }
    #[cfg(windows)]
    if !canonical
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            ["exe", "com", "bat", "cmd"]
                .iter()
                .any(|x| e.eq_ignore_ascii_case(x))
        })
    {
        return Err(refuse(
            "Select an executable file (.exe, .com, .bat or .cmd).",
        ));
    }
    Ok(canonical)
}

use crate::display_text::is_deceptive;

fn validate_args(args: &[String]) -> Result<(), AppError> {
    if args.len() > MAX_ARGS
        || args
            .iter()
            .any(|a| a.len() > MAX_ARG_LEN || a.chars().any(is_deceptive))
        || args.iter().map(String::len).sum::<usize>() > MAX_TOTAL_ARGS
    {
        return Err(refuse("The program arguments are not valid."));
    }
    Ok(())
}

/// What the user confirms: the program, then each argument on its own quoted line, so no argument
/// can merge into or hide behind another.
fn command_line_for_dialog(path: &str, args: &[String]) -> String {
    let mut out = path.to_owned();
    for a in args {
        out.push_str("\n  \"");
        out.push_str(&a.replace('\\', "\\\\").replace('"', "\\\""));
        out.push('"');
    }
    out
}

/// `ext` as stored: without the leading dot, lowercase, `[a-z0-9]{1,16}`.
fn normalize_ext(ext: &str) -> Result<String, AppError> {
    let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
    if crate::settings::valid_ext(&ext) {
        Ok(ext)
    } else {
        Err(refuse(
            "Enter a file extension made of letters and digits, such as docx.",
        ))
    }
}

/// Windows `canonicalize` returns `\\?\C:\...`, which `cmd.exe` based programs cannot run.
fn strip_verbatim(path: &str) -> String {
    match path.strip_prefix(r"\\?\") {
        Some(rest) if rest.len() > 2 && rest.as_bytes()[1] == b':' => rest.to_owned(),
        _ => path.to_owned(),
    }
}

/// Chooses and clears program settings.
pub struct ProgramService {
    settings: Arc<SettingsService>,
    picker: Arc<dyn ProgramPicker>,
    /// Held while a dialog is open: a second request cannot stack dialogs.
    dialog_open: Mutex<()>,
}

impl ProgramService {
    /// Service storing into `settings`, asking through `picker`.
    pub fn new(settings: Arc<SettingsService>, picker: Arc<dyn ProgramPicker>) -> Self {
        Self {
            settings,
            picker,
            dialog_open: Mutex::new(()),
        }
    }

    /// Lets the user pick a program and confirm `path args...`. Returns the settings after the
    /// attempt; they are unchanged when the user cancelled or declined.
    pub async fn choose(&self, kind: ProgramKind, args: Vec<String>) -> Result<Settings, AppError> {
        match self.pick_and_confirm(kind, &args, "").await? {
            Some(path) => self
                .settings
                .set_program(kind, Some(ExternalDiffTool { path, args })),
            None => Ok(self.settings.get()),
        }
    }

    /// Like [`Self::choose`] for the diff tool of files with extension `ext` (`.docx` or
    /// `docx`, any case). Replaces the tool already set for that extension.
    pub async fn choose_for_extension(
        &self,
        ext: &str,
        args: Vec<String>,
    ) -> Result<Settings, AppError> {
        let ext = normalize_ext(ext)?;
        let current = self
            .settings
            .get()
            .diff_tools_by_extension
            .unwrap_or_default();
        if current.len() >= MAX_EXT_TOOLS && !current.iter().any(|t| t.ext == ext) {
            return Err(refuse("Too many file types have their own diff tool."));
        }
        let context = format!("Diff tool for .{ext} files:\n");
        let Some(path) = self
            .pick_and_confirm(ProgramKind::DiffTool, &args, &context)
            .await?
        else {
            return Ok(self.settings.get());
        };
        // Read again: settings may have changed while the dialogs were open.
        let mut tools = self
            .settings
            .get()
            .diff_tools_by_extension
            .unwrap_or_default();
        tools.retain(|t| t.ext != ext);
        tools.push(ExtDiffTool { ext, path, args });
        self.settings.set_ext_tools(tools)
    }

    /// Removes the diff tool of extension `ext`.
    pub fn clear_for_extension(&self, ext: &str) -> Result<Settings, AppError> {
        let ext = normalize_ext(ext)?;
        let mut tools = self
            .settings
            .get()
            .diff_tools_by_extension
            .unwrap_or_default();
        tools.retain(|t| t.ext != ext);
        self.settings.set_ext_tools(tools)
    }

    /// Shows the native picker, then the confirmation of `context` + the command line. The
    /// canonical program path, or `None` when the user cancelled or declined.
    async fn pick_and_confirm(
        &self,
        kind: ProgramKind,
        args: &[String],
        context: &str,
    ) -> Result<Option<String>, AppError> {
        validate_args(args)?;
        let Ok(_open) = self.dialog_open.try_lock() else {
            return Err(refuse("A file picker is already open."));
        };
        let Some(picked) = self.picker.pick_program(kind).await else {
            return Ok(None);
        };
        let canonical = validate_program(&picked)?;
        let path = strip_verbatim(&canonical.to_string_lossy());
        if path.chars().any(is_deceptive) {
            return Err(refuse(
                "The program path contains characters that cannot be shown safely.",
            ));
        }
        let command_line = format!("{context}{}", command_line_for_dialog(&path, args));
        if !self.picker.confirm(kind, &command_line).await {
            return Ok(None);
        }
        Ok(Some(path))
    }

    /// The diff tool for a file at `path`: the one set for its extension, else
    /// `diff.externalTool`. Checked again before use like [`Self::external_diff_tool`]. The flag
    /// is true for a per-extension tool, which also receives binary files.
    pub fn external_diff_tool_for(
        &self,
        path: &str,
    ) -> Result<(PathBuf, Vec<String>, bool), AppError> {
        let ext = external_diff::staged_extension(path).map(|e| e.to_ascii_lowercase());
        let by_ext = ext.and_then(|ext| {
            self.settings
                .get()
                .diff_tools_by_extension
                .unwrap_or_default()
                .into_iter()
                .find(|t| t.ext == ext)
        });
        match by_ext {
            Some(tool) => {
                validate_args(&tool.args)?;
                let program = validate_program(Path::new(&tool.path))?;
                Ok((program, tool.args, true))
            }
            None => self
                .external_diff_tool()
                .map(|(program, args)| (program, args, false)),
        }
    }

    /// The configured external diff tool, checked again before use: the program must still exist
    /// and be executable, and the stored arguments must still pass validation.
    pub fn external_diff_tool(&self) -> Result<(PathBuf, Vec<String>), AppError> {
        let Some(tool) = self.settings.get().diff_external_tool else {
            let mut e = refuse("No external diff tool is configured. Choose one in Settings.");
            e.actions = Some(Box::new(vec![ErrorAction::OpenSettings]));
            return Err(e);
        };
        validate_args(&tool.args)?;
        let program = validate_program(Path::new(&tool.path))?;
        Ok((program, tool.args))
    }

    /// Removes the configured program. Clearing cannot launch anything, so no dialog is shown.
    pub fn clear(&self, kind: ProgramKind) -> Result<Settings, AppError> {
        self.settings.set_program(kind, None)
    }
}

#[cfg(test)]
mod tests;

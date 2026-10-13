//! Programs the app may launch (external diff/merge tool, editor, terminal).
//!
//! These settings are the one place where a compromised WebView could turn "change a setting"
//! into "run any executable". They therefore never go through `settings_set`: the user picks the
//! program in a native file dialog and confirms the exact command line in a native message
//! dialog; only then is the setting stored.

mod native;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use opensvn_ipc::{AppError, ExternalDiffTool, ProgramKind, Settings};
use tokio::sync::Mutex;

use crate::settings::SettingsService;

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
        validate_args(&args)?;
        let Ok(_open) = self.dialog_open.try_lock() else {
            return Err(refuse("A file picker is already open."));
        };
        let Some(picked) = self.picker.pick_program(kind).await else {
            return Ok(self.settings.get());
        };
        let canonical = validate_program(&picked)?;
        let path = strip_verbatim(&canonical.to_string_lossy());
        if path.chars().any(is_deceptive) {
            return Err(refuse(
                "The program path contains characters that cannot be shown safely.",
            ));
        }
        let command_line = command_line_for_dialog(&path, &args);
        if !self.picker.confirm(kind, &command_line).await {
            return Ok(self.settings.get());
        }
        self.settings
            .set_program(kind, Some(ExternalDiffTool { path, args }))
    }

    /// Removes the configured program. Clearing cannot launch anything, so no dialog is shown.
    pub fn clear(&self, kind: ProgramKind) -> Result<Settings, AppError> {
        self.settings.set_program(kind, None)
    }
}

#[cfg(test)]
mod tests;

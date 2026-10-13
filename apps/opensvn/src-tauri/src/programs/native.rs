//! Native dialogs (`rfd`). No JavaScript plugin is registered for them: the WebView can only
//! trigger them through the `program_choose` command and cannot read their result directly.

use std::path::PathBuf;

use async_trait::async_trait;
use opensvn_ipc::ProgramKind;
use rfd::{AsyncFileDialog, AsyncMessageDialog, MessageButtons, MessageDialogResult, MessageLevel};

use super::ProgramPicker;

/// Picker backed by the operating system's dialogs.
pub struct NativeProgramPicker;

fn label(kind: ProgramKind) -> &'static str {
    match kind {
        ProgramKind::DiffTool => "external diff tool",
        ProgramKind::MergeTool => "external merge tool",
        ProgramKind::Editor => "editor",
        ProgramKind::Terminal => "terminal",
    }
}

#[async_trait]
impl ProgramPicker for NativeProgramPicker {
    async fn pick_program(&self, kind: ProgramKind) -> Option<PathBuf> {
        AsyncFileDialog::new()
            .set_title(format!("Select the {}", label(kind)))
            .pick_file()
            .await
            .map(|h| h.path().to_path_buf())
    }

    async fn confirm(&self, kind: ProgramKind, command_line: &str) -> bool {
        AsyncMessageDialog::new()
            .set_level(MessageLevel::Warning)
            .set_title("Allow OpenSVN to run this program?")
            .set_description(format!(
                "OpenSVN will run this {} when you use it:\n\n{command_line}\n\nOnly continue if you chose this program yourself.",
                label(kind)
            ))
            .set_buttons(MessageButtons::YesNo)
            .show()
            .await
            == MessageDialogResult::Yes
    }
}

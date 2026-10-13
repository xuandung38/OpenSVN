//! Native file and folder dialogs (`rfd`).

use std::path::PathBuf;

use async_trait::async_trait;
use opensvn_ipc::{GrantKind, PickRequest};
use rfd::AsyncFileDialog;

use super::PathPicker;

/// Picker backed by the operating system's dialogs.
pub struct NativePathPicker;

#[async_trait]
impl PathPicker for NativePathPicker {
    async fn pick(&self, request: &PickRequest) -> Option<(PathBuf, GrantKind)> {
        match request {
            PickRequest::Folder { title } => {
                let mut d = AsyncFileDialog::new();
                if let Some(t) = title {
                    d = d.set_title(t.clone());
                }
                d.pick_folder()
                    .await
                    .map(|h| (h.path().to_path_buf(), GrantKind::Dir))
            }
            PickRequest::File { title } => {
                let mut d = AsyncFileDialog::new();
                if let Some(t) = title {
                    d = d.set_title(t.clone());
                }
                d.pick_file()
                    .await
                    .map(|h| (h.path().to_path_buf(), GrantKind::File))
            }
            PickRequest::SaveFile {
                suggested_name,
                title,
            } => {
                let mut d = AsyncFileDialog::new().set_file_name(suggested_name.clone());
                if let Some(t) = title {
                    d = d.set_title(t.clone());
                }
                d.save_file()
                    .await
                    .map(|h| (h.path().to_path_buf(), GrantKind::File))
            }
        }
    }
}

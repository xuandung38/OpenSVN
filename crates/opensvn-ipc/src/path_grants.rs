//! Path grants: how the UI names a local file or folder outside any working copy.
//!
//! The WebView never sends raw filesystem paths for reads or writes outside a registered working
//! copy. Such paths (checkout target, save-as target, upload source, working copy to add) come from
//! a native dialog (`path_pick`) or a native drag-and-drop (`fs://dropped`). Rust keeps the real
//! path and hands out an opaque [`PathGrantId`]: single use, valid for 5 minutes, bound to the
//! `main` window. Commands take the id instead of a path.

use serde::{Deserialize, Serialize};
use specta::Type;

/// Opaque, single-use, time-limited handle to a path chosen by the user.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(transparent)]
pub struct PathGrantId(pub String);

/// What a grant points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum GrantKind {
    File,
    Dir,
}

/// A grant as shown to the UI: the id and a label to display. The real path is not included.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PathGrantInfo {
    pub id: PathGrantId,
    /// Last path component, for display only.
    pub display_name: String,
    pub kind: GrantKind,
}

/// Which native dialog `path_pick` shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PickRequest {
    /// Choose an existing folder (checkout target, working copy to add).
    Folder { title: Option<String> },
    /// Choose an existing file (upload source).
    File { title: Option<String> },
    /// Choose where to save a new file (`repo_save`).
    SaveFile {
        suggested_name: String,
        title: Option<String>,
    },
}

/// Drop position in logical pixels relative to the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DropPosition {
    pub x: i32,
    pub y: i32,
}

/// Payload of `fs://dropped`: one grant per dropped item. The raw paths of a drop are never
/// delivered to the WebView.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DroppedPaths {
    pub grants: Vec<PathGrantInfo>,
    pub position: DropPosition,
}

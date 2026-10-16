//! Event bindings. Payload types live in `opensvn-ipc`; these transparent wrappers bind them to
//! the event names from docs/system-design/03 section 5.

use opensvn_ipc::{
    AuthPrompt, Banner, DroppedPaths, Incoming, OpFailed, OpFinished, OperationHandle, Progress,
    RevisionChanged, StatusChanged,
};
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri_specta::Event;

macro_rules! event {
    ($name:ident, $payload:ty, $event_name:literal) => {
        #[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
        #[serde(transparent)]
        #[tauri_specta(event_name = $event_name)]
        pub struct $name(pub $payload);
    };
}

event!(OpStarted, OperationHandle, "op://started");
event!(OpProgress, Progress, "op://progress");
event!(OpFinishedEvent, OpFinished, "op://finished");
event!(OpFailedEvent, OpFailed, "op://failed");
event!(WcStatusChanged, StatusChanged, "wc://status-changed");
event!(WcRevisionChanged, RevisionChanged, "wc://revision-changed");
event!(WcIncoming, Incoming, "wc://incoming");
event!(WcBanner, Banner, "wc://banner");
event!(AuthPromptEvent, AuthPrompt, "auth://prompt");
event!(FsDropped, DroppedPaths, "fs://dropped");
event!(
    AppUpdateProgress,
    crate::updater::UpdateProgress,
    "app://update-progress"
);

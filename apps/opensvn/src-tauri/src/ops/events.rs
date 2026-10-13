//! Outbound events (Rust -> UI) behind a trait, so services are testable without a window.

use opensvn_ipc::{
    Banner, Incoming, OpFailed, OpFinished, OperationHandle, Progress, RevisionChanged,
    StatusChanged,
};
use tauri_specta::Event;

use crate::events::{
    OpFailedEvent, OpFinishedEvent, OpProgress, OpStarted, WcBanner, WcIncoming, WcRevisionChanged,
    WcStatusChanged,
};

/// Everything the backend pushes to the UI. Implementations must not block.
pub trait AppEvents: Send + Sync + 'static {
    /// `op://started`
    fn op_started(&self, handle: &OperationHandle);
    /// `op://progress` (already throttled by the caller).
    fn op_progress(&self, progress: &Progress);
    /// `op://finished`
    fn op_finished(&self, finished: &OpFinished);
    /// `op://failed`
    fn op_failed(&self, failed: &OpFailed);
    /// `wc://status-changed`
    fn status_changed(&self, changed: &StatusChanged);
    /// `wc://revision-changed`
    fn revision_changed(&self, changed: &RevisionChanged);
    /// `wc://incoming`
    fn incoming(&self, incoming: &Incoming);
    /// `wc://banner`
    fn banner(&self, banner: &Banner);
}

/// Emits events to the WebView through Tauri.
pub struct TauriEvents<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
}

impl<R: tauri::Runtime> TauriEvents<R> {
    /// Wraps an app handle.
    pub fn new(app: tauri::AppHandle<R>) -> Self {
        Self { app }
    }
}

macro_rules! emit {
    ($self:ident, $wrapper:ident, $payload:expr, $name:literal) => {
        if let Err(e) = $wrapper($payload.clone()).emit(&$self.app) {
            tracing::warn!(event = $name, error = %e, "failed to emit event");
        }
    };
}

impl<R: tauri::Runtime> AppEvents for TauriEvents<R> {
    fn op_started(&self, h: &OperationHandle) {
        emit!(self, OpStarted, h, "op://started");
    }
    fn op_progress(&self, p: &Progress) {
        emit!(self, OpProgress, p, "op://progress");
    }
    fn op_finished(&self, f: &OpFinished) {
        emit!(self, OpFinishedEvent, f, "op://finished");
    }
    fn op_failed(&self, f: &OpFailed) {
        emit!(self, OpFailedEvent, f, "op://failed");
    }
    fn status_changed(&self, c: &StatusChanged) {
        emit!(self, WcStatusChanged, c, "wc://status-changed");
    }
    fn revision_changed(&self, c: &RevisionChanged) {
        emit!(self, WcRevisionChanged, c, "wc://revision-changed");
    }
    fn incoming(&self, i: &Incoming) {
        emit!(self, WcIncoming, i, "wc://incoming");
    }
    fn banner(&self, b: &Banner) {
        emit!(self, WcBanner, b, "wc://banner");
    }
}

/// One recorded event.
#[cfg(test)]
#[derive(Debug, Clone)]
pub enum Recorded {
    Started(OperationHandle),
    Progress(Progress),
    Finished(OpFinished),
    Failed(OpFailed),
    Status(StatusChanged),
    Revision(RevisionChanged),
    Incoming(Incoming),
    Banner(Banner),
}

/// Collects events for assertions in tests.
#[cfg(test)]
#[derive(Default)]
pub struct RecordingEvents {
    log: std::sync::Mutex<Vec<Recorded>>,
}

#[cfg(test)]
impl RecordingEvents {
    fn push(&self, r: Recorded) {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(r);
    }

    /// Everything recorded so far, in emission order.
    pub fn all(&self) -> Vec<Recorded> {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Event names in emission order (`started:<op>`, `finished:<op>`, ...).
    pub fn names(&self) -> Vec<String> {
        self.all()
            .iter()
            .map(|r| match r {
                Recorded::Started(h) => format!("started:{}", h.op_id.0),
                Recorded::Progress(p) => format!("progress:{}", p.op_id.0),
                Recorded::Finished(f) => format!("finished:{}", f.op_id.0),
                Recorded::Failed(f) => format!("failed:{}", f.op_id.0),
                Recorded::Status(c) => format!("status:{}", c.wc_id.0),
                Recorded::Revision(c) => format!("revision:{}", c.wc_id.0),
                Recorded::Incoming(i) => format!("incoming:{}", i.wc_id.0),
                Recorded::Banner(b) => format!("banner:{}", b.wc_id.0),
            })
            .collect()
    }

    /// Progress payloads in emission order.
    pub fn progress(&self) -> Vec<Progress> {
        self.all()
            .into_iter()
            .filter_map(|r| {
                if let Recorded::Progress(p) = r {
                    Some(p)
                } else {
                    None
                }
            })
            .collect()
    }
}

#[cfg(test)]
impl AppEvents for RecordingEvents {
    fn op_started(&self, h: &OperationHandle) {
        self.push(Recorded::Started(h.clone()));
    }
    fn op_progress(&self, p: &Progress) {
        self.push(Recorded::Progress(p.clone()));
    }
    fn op_finished(&self, f: &OpFinished) {
        self.push(Recorded::Finished(f.clone()));
    }
    fn op_failed(&self, f: &OpFailed) {
        self.push(Recorded::Failed(f.clone()));
    }
    fn status_changed(&self, c: &StatusChanged) {
        self.push(Recorded::Status(c.clone()));
    }
    fn revision_changed(&self, c: &RevisionChanged) {
        self.push(Recorded::Revision(c.clone()));
    }
    fn incoming(&self, i: &Incoming) {
        self.push(Recorded::Incoming(i.clone()));
    }
    fn banner(&self, b: &Banner) {
        self.push(Recorded::Banner(b.clone()));
    }
}

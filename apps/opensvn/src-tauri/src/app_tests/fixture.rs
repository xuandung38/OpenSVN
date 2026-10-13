#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use opensvn_ipc::{AppError, OpResult, OperationHandle, ProgramKind, WorkingCopy};
use tauri::Manager;
use tauri::test::MockRuntime;

use crate::app_state::{AppDeps, AppState};
use crate::engine::FakeEngine;
use crate::engine::fake::demo_wc_root;
use crate::ops::events::{Recorded, RecordingEvents};
use crate::programs::ProgramPicker;
use crate::status::StatusTiming;

/// Dialog double with scripted answers.
#[derive(Default)]
pub struct Scripted {
    pub pick: Mutex<Option<PathBuf>>,
    pub accept: AtomicBool,
    pub confirmations: Mutex<Vec<String>>,
}

#[async_trait]
impl ProgramPicker for Scripted {
    async fn pick_program(&self, _kind: ProgramKind) -> Option<PathBuf> {
        self.pick.lock().unwrap().clone()
    }
    async fn confirm(&self, _kind: ProgramKind, command_line: &str) -> bool {
        self.confirmations
            .lock()
            .unwrap()
            .push(command_line.to_owned());
        self.accept.load(Ordering::SeqCst)
    }
}

pub struct Fixture {
    pub app: tauri::App<MockRuntime>,
    pub events: Arc<RecordingEvents>,
    pub picker: Arc<Scripted>,
    pub engine: Arc<FakeEngine>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// App state over the demo fake engine with `delay` per simulated item.
    pub async fn new(delay: Duration) -> Self {
        Self::with_timing(delay, StatusTiming::default()).await
    }

    /// Like [`Fixture::new`] with custom watcher timing.
    pub async fn with_timing(delay: Duration, status_timing: StatusTiming) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(FakeEngine::demo(delay).await.unwrap());
        let events = Arc::new(RecordingEvents::default());
        let picker = Arc::new(Scripted::default());
        let state = AppState::build(AppDeps {
            data_dir: dir.path().to_path_buf(),
            engine: engine.clone(),
            events: events.clone(),
            picker: picker.clone(),
            status_timing,
        })
        .unwrap();
        let app = tauri::test::mock_app();
        app.manage(state);
        Self {
            app,
            events,
            picker,
            engine,
            _dir: dir,
        }
    }

    pub fn state(&self) -> tauri::State<'_, AppState> {
        self.app.state::<AppState>()
    }

    /// Registers the demo working copy.
    pub async fn add_demo_wc(&self) -> WorkingCopy {
        crate::commands::wc_add_existing(
            self.state(),
            demo_wc_root().to_string_lossy().into_owned(),
        )
        .await
        .unwrap()
    }

    /// Waits for the terminal event of `handle` and returns its outcome.
    pub async fn wait_op(&self, handle: &OperationHandle) -> Result<OpResult, AppError> {
        for _ in 0..1000 {
            for r in self.events.all() {
                match r {
                    Recorded::Finished(f) if f.op_id == handle.op_id => return Ok(f.result),
                    Recorded::Failed(f) if f.op_id == handle.op_id => return Err(f.error),
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("operation {} did not end", handle.op_id.0);
    }
}

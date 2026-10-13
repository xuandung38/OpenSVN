//! Settings service: `settings_get` / `settings_set` with three-state patch semantics.
//!
//! Settings that make the app launch a program (external diff/merge tool, editor, terminal) are
//! deliberately not writable through a patch: a compromised WebView could otherwise point them at an
//! arbitrary executable. They change only through [`SettingsService::set_program`], which the
//! native file-picker command calls after the user confirmed in a native dialog.

mod defaults;
mod validate;

use std::sync::Mutex;

use opensvn_ipc::{AppError, ExternalDiffTool, ProgramKind, Settings, SettingsPatch};
use serde_json::{Map, Value};
use tokio::sync::watch;

use crate::db::AppDb;

pub use defaults::{defaults, system_locale};

/// Dotted keys that launch programs. A test asserts this list equals the set of nullable keys of
/// [`Settings`], so a new program-launching setting cannot be forgotten. `SettingsPatch` has none
/// of them.
pub const PROGRAM_KEYS: [&str; 4] = [
    "diff.externalTool",
    "diff.mergeTool",
    "tools.editor",
    "tools.terminal",
];

fn program_key(kind: ProgramKind) -> &'static str {
    match kind {
        ProgramKind::DiffTool => "diff.externalTool",
        ProgramKind::MergeTool => "diff.mergeTool",
        ProgramKind::Editor => "tools.editor",
        ProgramKind::Terminal => "tools.terminal",
    }
}

fn to_map(s: &Settings) -> Result<Map<String, Value>, AppError> {
    match serde_json::to_value(s) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(AppError::internal("settings could not be serialized")),
    }
}

fn from_map(m: Map<String, Value>) -> Result<Settings, AppError> {
    serde_json::from_value(Value::Object(m))
        .map_err(|e| AppError::internal("Invalid settings value.").with_detail(e.to_string()))
}

/// Owns the effective settings and broadcasts every change to interested services.
pub struct SettingsService {
    db: AppDb,
    /// Serializes read-modify-write cycles.
    write_lock: Mutex<()>,
    tx: watch::Sender<Settings>,
}

impl SettingsService {
    /// Loads stored settings over the defaults. A stored value that no longer parses or
    /// validates is ignored (the default applies) rather than blocking start-up.
    pub fn load(db: AppDb, locale: Option<&str>) -> Result<Self, AppError> {
        let mut map = to_map(&defaults(locale))?;
        for (key, raw) in db.settings_all()? {
            let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                tracing::warn!(key, "ignoring unparsable stored setting");
                continue;
            };
            let Some(slot) = map.get_mut(&key) else {
                continue; // key from another version
            };
            let previous = std::mem::replace(slot, value);
            let ok = from_map(map.clone())
                .and_then(|s| validate::validate(&s))
                .is_ok();
            if !ok {
                tracing::warn!(key, "ignoring invalid stored setting");
                map.insert(key, previous);
            }
        }
        let current = from_map(map)?;
        let (tx, _rx) = watch::channel(current);
        Ok(Self {
            db,
            write_lock: Mutex::new(()),
            tx,
        })
    }

    /// Current effective settings.
    pub fn get(&self) -> Settings {
        self.tx.borrow().clone()
    }

    /// Receiver that observes every accepted change (poller interval, connection limits, ...).
    pub fn subscribe(&self) -> watch::Receiver<Settings> {
        self.tx.subscribe()
    }

    /// Applies a patch from the UI. `SettingsPatch` cannot carry program-launching keys (they
    /// exist in `Settings` only and unknown keys fail to deserialize), so none can arrive here.
    pub fn apply_patch(&self, patch: &SettingsPatch) -> Result<Settings, AppError> {
        let patch_map = match serde_json::to_value(patch) {
            Ok(Value::Object(m)) => m,
            _ => return Err(AppError::internal("settings patch could not be serialized")),
        };
        self.change(patch_map)
    }

    /// Stores or clears (`None`) a program setting. Callers must have obtained the user's
    /// confirmation through a native dialog first.
    pub fn set_program(
        &self,
        kind: ProgramKind,
        tool: Option<ExternalDiffTool>,
    ) -> Result<Settings, AppError> {
        let value = match tool {
            Some(t) => serde_json::to_value(t).map_err(|e| {
                AppError::internal("program could not be serialized").with_detail(e.to_string())
            })?,
            None => Value::Null,
        };
        let mut changes = Map::new();
        changes.insert(program_key(kind).to_owned(), value);
        self.change(changes)
    }

    /// Validates `changes` against the current settings, persists them and notifies subscribers.
    fn change(&self, changes: Map<String, Value>) -> Result<Settings, AppError> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut map = to_map(&self.get())?;
        let mut rows = Vec::with_capacity(changes.len());
        for (key, value) in changes {
            rows.push((
                key.clone(),
                if value.is_null() {
                    None
                } else {
                    Some(value.to_string())
                },
            ));
            map.insert(key, value);
        }
        let next = from_map(map)?;
        validate::validate(&next)?;
        self.db.settings_apply(&rows)?;
        self.tx.send_replace(next.clone());
        Ok(next)
    }
}

#[cfg(test)]
mod tests;

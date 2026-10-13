//! OS notifications. Shown from Rust only; the WebView gets no notification permission.

use tauri_plugin_notification::NotificationExt;

/// Shows a desktop notification.
pub trait Notifier: Send + Sync + 'static {
    /// Shows `title` and `body`; failures are logged, never surfaced.
    fn notify(&self, title: &str, body: &str);
}

/// Notifications through the Tauri notification plugin.
pub struct TauriNotifier<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
}

impl<R: tauri::Runtime> TauriNotifier<R> {
    /// Wraps an app handle.
    pub fn new(app: tauri::AppHandle<R>) -> Self {
        Self { app }
    }
}

impl<R: tauri::Runtime> Notifier for TauriNotifier<R> {
    fn notify(&self, title: &str, body: &str) {
        if let Err(e) = self
            .app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show()
        {
            tracing::warn!(error = %e, "could not show a notification");
        }
    }
}

/// Text of the "new revisions" notification.
pub fn incoming_text(name: &str, count: u32) -> (String, String) {
    let body = if count == 1 {
        "1 new revision is available.".to_owned()
    } else {
        format!("{count} new revisions are available.")
    };
    (format!("{name}: updates available"), body)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn wording_is_singular_and_plural() {
        assert_eq!(incoming_text("demo", 1).1, "1 new revision is available.");
        assert_eq!(incoming_text("demo", 3).1, "3 new revisions are available.");
        assert_eq!(incoming_text("demo", 2).0, "demo: updates available");
    }
}

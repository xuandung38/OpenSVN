//! OS notifications. Shown from Rust only; the WebView gets no notification permission.

use tauri_plugin_notification::NotificationExt;

use crate::display_text::escape_for_display;

/// Longest notification title shown, in characters.
const MAX_TITLE_CHARS: usize = 120;
/// Longest notification body shown, in characters.
const MAX_BODY_CHARS: usize = 400;

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
            .title(escape_for_display(title, MAX_TITLE_CHARS))
            .body(escape_for_display(body, MAX_BODY_CHARS))
            .show()
        {
            tracing::warn!(error = %e, "could not show a notification");
        }
    }
}

/// Text of the "new revisions" notification. The working copy name is user data: bidirectional and
/// invisible characters in it are shown as escapes so they cannot reorder the title.
pub fn incoming_text(name: &str, count: u32) -> (String, String) {
    let name = escape_for_display(name, 80);
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

    #[test]
    fn bidi_controls_in_the_working_copy_name_are_escaped() {
        let (title, _) = incoming_text("trunk\u{202E}ecnanif", 2);
        assert_eq!(title, "trunk\\u{202E}ecnanif: updates available");
        assert!(!crate::display_text::has_deceptive(&title));
        let (title, _) = incoming_text("a\nb\u{2028}c\u{2066}", 1);
        assert!(!crate::display_text::has_deceptive(&title), "{title}");
    }
}

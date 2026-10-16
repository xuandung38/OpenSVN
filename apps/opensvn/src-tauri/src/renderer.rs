//! Hardware acceleration of the window's web view (Linux).
//!
//! WebKitGTK composites the page with the GPU by default (the DMA-BUF renderer since 2.42). On a
//! machine without a usable GPU driver (a VM, a remote X session, Xvfb) Mesa falls back to
//! llvmpipe: both the app and its web process then load LLVM and keep software GL buffers, about
//! 160 MB more resident memory than without acceleration (`benches/results/`). Turning it off
//! paints the page in software instead: less memory and no GL at all, but more CPU when large
//! areas repaint (scrolling long lists, animations). Some GPU drivers (NVIDIA on Wayland) show a
//! blank window with acceleration; turning it off is the usual fix there too.
//!
//! `appearance.hardwareAcceleration` (default on) decides, applied when the window opens; the
//! environment variable [`HW_ACCEL_ENV`] overrides it, so a window that stays blank can still be
//! started without acceleration to reach the setting. WebKit's own `WEBKIT_DISABLE_*` variables
//! keep working as before. macOS and Windows ignore the setting.

/// `0`/`false`/`off` turns hardware acceleration off, `1`/`true`/`on` turns it on, whatever the
/// setting says.
pub const HW_ACCEL_ENV: &str = "OPENSVN_HARDWARE_ACCELERATION";

/// Whether the web view may use the GPU: the environment override (when set to a known value),
/// else the setting.
pub fn hardware_acceleration(setting: bool, env: Option<&str>) -> bool {
    match env.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("0" | "false" | "off" | "no") => false,
        Some("1" | "true" | "on" | "yes") => true,
        _ => setting,
    }
}

/// Applies the policy to every web view window of `app`. Nothing changes while acceleration is
/// on: WebKit's default (and its environment variables) stay in charge.
pub fn apply<R: tauri::Runtime>(app: &tauri::App<R>, setting: bool) {
    let enabled = hardware_acceleration(setting, std::env::var(HW_ACCEL_ENV).ok().as_deref());
    tracing::info!(enabled, "web view hardware acceleration");
    if enabled {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        use tauri::Manager;
        for (label, window) in app.webview_windows() {
            let applied = window.with_webview(|webview| {
                use webkit2gtk::{HardwareAccelerationPolicy, SettingsExt, WebViewExt};
                if let Some(settings) = WebViewExt::settings(&webview.inner()) {
                    settings.set_hardware_acceleration_policy(HardwareAccelerationPolicy::Never);
                }
            });
            if let Err(e) = applied {
                tracing::warn!(label, error = %e, "could not turn off hardware acceleration");
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = app;
}

#[cfg(test)]
mod tests {
    use super::hardware_acceleration;

    #[test]
    fn the_environment_overrides_the_setting_only_with_a_known_value() {
        assert!(hardware_acceleration(true, None));
        assert!(!hardware_acceleration(false, None));
        for off in ["0", "false", "OFF", " no "] {
            assert!(!hardware_acceleration(true, Some(off)), "{off}");
        }
        for on in ["1", "true", "On", "yes"] {
            assert!(hardware_acceleration(false, Some(on)), "{on}");
        }
        assert!(hardware_acceleration(true, Some("maybe")));
        assert!(!hardware_acceleration(false, Some("")));
    }
}

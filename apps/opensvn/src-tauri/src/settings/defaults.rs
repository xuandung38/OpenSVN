//! Default values of every setting (docs/system-design/03 section 6.3).

use opensvn_ipc::{DiffMode, Language, ProxySetting, Settings, Theme, WcFormat};

/// Default for `integrations.webRevisionUrl`.
pub const DEFAULT_WEB_REVISION_URL: &str = "https://{server}/!/#{repo}/commit/r{rev}/";
/// Default for `integrations.webFileUrl`.
pub const DEFAULT_WEB_FILE_URL: &str = "https://{server}/!/#{repo}/view/{revSpec}/{path}";

/// UI language chosen from a locale string such as `vi_VN.UTF-8`; English otherwise.
pub fn language_from_locale(locale: Option<&str>) -> Language {
    match locale {
        Some(l) if l.to_ascii_lowercase().starts_with("vi") => Language::Vi,
        _ => Language::En,
    }
}

/// Effective settings when nothing is stored.
pub fn defaults(locale: Option<&str>) -> Settings {
    Settings {
        general_checkout_root: "~/svn".to_owned(),
        general_check_interval_min: 10,
        general_language: language_from_locale(locale),
        appearance_theme: Theme::System,
        diff_mode: DiffMode::Split,
        diff_ignore_whitespace: false,
        network_max_connections: 4,
        network_timeout_sec: 60,
        network_proxy: ProxySetting::System,
        network_negotiate: true,
        wc_default_format: WcFormat::V31,
        wc_read_subversion_config: true,
        cache_max_size_gb: 2,
        integrations_web_revision_url: DEFAULT_WEB_REVISION_URL.to_owned(),
        integrations_web_file_url: DEFAULT_WEB_FILE_URL.to_owned(),
        diff_external_tool: None,
        diff_merge_tool: None,
        tools_editor: None,
        tools_terminal: None,
    }
}

/// OS locale from the environment (`LC_ALL`, `LC_MESSAGES`, `LANG`), if any.
pub fn system_locale() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
}

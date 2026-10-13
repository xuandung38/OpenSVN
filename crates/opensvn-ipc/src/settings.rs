//! Application settings (docs/system-design/03 section 6.3).
//!
//! `Settings` carries every key with its dotted name as the JSON/TS property;
//! `SettingsPatch` has the same keys, all optional, for partial updates.

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::models::WcFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Vi,
    En,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum DiffMode {
    Split,
    Unified,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExternalDiffTool {
    pub path: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum ProxySetting {
    System,
    Custom { url: String },
}

macro_rules! settings_structs {
    ($( $(#[$m:meta])* $field:ident : $ty:ty = $key:literal ),+ $(,)?) => {
        /// All settings with their effective values.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
        pub struct Settings {
            $( $(#[$m])* #[serde(rename = $key)] pub $field: $ty, )+
        }

        /// Partial update: absent keys are left unchanged.
        #[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, Type)]
        pub struct SettingsPatch {
            $(
                $(#[$m])*
                #[serde(rename = $key, default, skip_serializing_if = "Option::is_none")]
                #[specta(optional)]
                pub $field: Option<$ty>,
            )+
        }
    };
}

settings_structs! {
    general_checkout_root: String = "general.checkoutRoot",
    general_check_interval_min: u32 = "general.checkIntervalMin",
    general_language: Language = "general.language",
    appearance_theme: Theme = "appearance.theme",
    diff_mode: DiffMode = "diff.mode",
    diff_ignore_whitespace: bool = "diff.ignoreWhitespace",
    /// `null` when no external tool is configured.
    diff_external_tool: Option<ExternalDiffTool> = "diff.externalTool",
    /// 1..=16
    network_max_connections: u8 = "network.maxConnections",
    network_timeout_sec: u32 = "network.timeoutSec",
    network_proxy: ProxySetting = "network.proxy",
    wc_default_format: WcFormat = "wc.defaultFormat",
    wc_read_subversion_config: bool = "wc.readSubversionConfig",
    cache_max_size_gb: u32 = "cache.maxSizeGb",
    integrations_web_revision_url: String = "integrations.webRevisionUrl",
    integrations_web_file_url: String = "integrations.webFileUrl",
}

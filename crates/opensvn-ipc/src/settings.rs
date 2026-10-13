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

/// Settings that make the app launch a program. They are changed only through a native file
/// picker plus confirmation (`program_choose` / `program_clear`), never through `settings_set`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum ProgramKind {
    DiffTool,
    MergeTool,
    Editor,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum ProxySetting {
    System,
    Custom { url: String },
}

/// Deserializes a present key (including an explicit `null`) as `Some(..)`; combined with
/// `#[serde(default)]` an absent key stays `None`. This is what separates "clear" from "unchanged".
fn present_or_absent<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// `plain` keys hold a value; `nullable` keys may also be cleared; `program` keys exist in
/// `Settings` only (they launch programs and change through the native picker, never a patch). In `SettingsPatch` a plain key
/// is `absent = unchanged | value = set`, a nullable key is `absent = unchanged | null = clear |
/// value = set` (`Option<Option<T>>`, exported to TS as an optional `T | null`).
macro_rules! settings_structs {
    (
        plain { $( $(#[$pm:meta])* $pf:ident : $pt:ty = $pk:literal ),+ $(,)? }
        nullable { $( $(#[$nm:meta])* $nf:ident : $nt:ty = $nk:literal ),* $(,)? }
        program { $( $(#[$gm:meta])* $gf:ident : $gt:ty = $gk:literal ),+ $(,)? }
    ) => {
        /// All settings with their effective values.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
        pub struct Settings {
            $( $(#[$pm])* #[serde(rename = $pk)] pub $pf: $pt, )+
            $( $(#[$nm])* #[serde(rename = $nk)] pub $nf: Option<$nt>, )*
            $( $(#[$gm])* #[serde(rename = $gk)] pub $gf: Option<$gt>, )+
        }

        /// Partial update: absent keys are left unchanged; nullable keys accept `null` to clear.
        ///
        /// Unknown keys are rejected, so a program-launching key (which exists in [`Settings`] only)
        /// fails loudly instead of being ignored.
        #[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, Type)]
        #[serde(deny_unknown_fields)]
        pub struct SettingsPatch {
            $(
                $(#[$pm])*
                #[serde(rename = $pk, default, skip_serializing_if = "Option::is_none")]
                #[specta(optional)]
                pub $pf: Option<$pt>,
            )+
            $(
                $(#[$nm])*
                #[serde(
                    rename = $nk,
                    default,
                    deserialize_with = "present_or_absent",
                    skip_serializing_if = "Option::is_none"
                )]
                #[specta(optional, type = Option<$nt>)]
                pub $nf: Option<Option<$nt>>,
            )*
        }
    };
}

settings_structs! {
    plain {
        general_checkout_root: String = "general.checkoutRoot",
        general_check_interval_min: u32 = "general.checkIntervalMin",
        general_language: Language = "general.language",
        appearance_theme: Theme = "appearance.theme",
        diff_mode: DiffMode = "diff.mode",
        diff_ignore_whitespace: bool = "diff.ignoreWhitespace",
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
    nullable {}
    program {
        /// `null` when no external tool is configured.
        diff_external_tool: ExternalDiffTool = "diff.externalTool",
        /// Same shape as `diff.externalTool`. `null` when not configured.
        diff_merge_tool: ExternalDiffTool = "diff.mergeTool",
        /// Editor used by "open with editor". `null` when not configured.
        tools_editor: ExternalDiffTool = "tools.editor",
        /// Terminal used by "open terminal here". `null` when not configured.
        tools_terminal: ExternalDiffTool = "tools.terminal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three-state mechanism for nullable patch keys (no key uses it right now).
    #[derive(Debug, Deserialize)]
    struct Probe {
        #[serde(default, deserialize_with = "present_or_absent")]
        key: Option<Option<u32>>,
    }

    #[test]
    fn present_or_absent_separates_unchanged_clear_and_set() {
        let absent: Probe = serde_json::from_str("{}").expect("json");
        let clear: Probe = serde_json::from_str(r#"{"key":null}"#).expect("json");
        let set: Probe = serde_json::from_str(r#"{"key":3}"#).expect("json");
        assert_eq!(absent.key, None);
        assert_eq!(clear.key, Some(None));
        assert_eq!(set.key, Some(Some(3)));
    }
}

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
// Used by `nullable` patch keys; none exist right now (program keys are `Settings`-only).
#[allow(dead_code)]
fn present_or_absent<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// `plain` keys hold a value; `nullable` keys may also be cleared; `picked` keys hold a local
/// folder and `program` keys a program to launch: both exist in `Settings` only and change through a
/// native picker, never a patch (a WebView must not choose where the app writes or what it runs). In
/// `SettingsPatch` a plain key
/// is `absent = unchanged | value = set`, a nullable key is `absent = unchanged | null = clear |
/// value = set` (`Option<Option<T>>`, exported to TS as an optional `T | null`).
macro_rules! settings_structs {
    (
        plain { $( $(#[$pm:meta])* $pf:ident : $pt:ty = $pk:literal ),+ $(,)? }
        nullable { $( $(#[$nm:meta])* $nf:ident : $nt:ty = $nk:literal ),* $(,)? }
        picked { $( $(#[$km:meta])* $kf:ident : $kt:ty = $kk:literal ),+ $(,)? }
        program { $( $(#[$gm:meta])* $gf:ident : $gt:ty = $gk:literal ),+ $(,)? }
    ) => {
        /// All settings with their effective values.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
        pub struct Settings {
            $( $(#[$pm])* #[serde(rename = $pk)] pub $pf: $pt, )+
            $( $(#[$nm])* #[serde(rename = $nk)] pub $nf: Option<$nt>, )*
            $( $(#[$km])* #[serde(rename = $kk)] pub $kf: $kt, )+
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
        general_check_interval_min: u32 = "general.checkIntervalMin",
        general_language: Language = "general.language",
        appearance_theme: Theme = "appearance.theme",
        diff_mode: DiffMode = "diff.mode",
        diff_ignore_whitespace: bool = "diff.ignoreWhitespace",
        /// 1..=16
        network_max_connections: u8 = "network.maxConnections",
        network_timeout_sec: u32 = "network.timeoutSec",
        network_proxy: ProxySetting = "network.proxy",
        /// Kerberos single sign-on (Negotiate) when the server offers it; off falls back to a
        /// user name and password.
        network_negotiate: bool = "network.negotiate",
        wc_default_format: WcFormat = "wc.defaultFormat",
        wc_read_subversion_config: bool = "wc.readSubversionConfig",
        cache_max_size_gb: u32 = "cache.maxSizeGb",
        integrations_web_revision_url: String = "integrations.webRevisionUrl",
        integrations_web_file_url: String = "integrations.webFileUrl",
    }
    nullable {}
    picked {
        /// Parent folder offered for new checkouts. `~` stands for the home folder. Changes only
        /// through `checkout_root_choose` (native folder dialog).
        general_checkout_root: String = "general.checkoutRoot",
    }
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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

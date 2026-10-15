//! Semantic validation of a complete settings value.

use opensvn_ipc::{
    AppError, ExtDiffTool, ExternalDiffTool, ProxySetting, SettingRange, Settings, WcFormat,
};
use serde_json::Value;

/// Every numeric setting and its inclusive range. This table is the only definition: [`validate`]
/// checks it, [`clamp_stored`] applies it to stored values, and the UI reads it from the generated
/// bindings (`SETTING_RANGES`) for its own checks, labels and `<input min/max>`.
///
/// - `general.checkIntervalMin`: minutes between background checks; the poller has no "off".
/// - `network.maxConnections`: the HTTP transport supports 1..=8 per host
///   (`svn_ra::transport::config::MAX_CONNECTIONS`); default 4, like svn.
/// - `network.timeoutSec`: below 5 s a slow TLS handshake already fails.
/// - `network.reconnectWaitMin`: how long a checkout or update waits for a lost connection;
///   up to 2 hours (a long outage, a laptop closed for a meeting).
/// - `cache.maxSizeGb`: up to 4 TB.
pub const SETTING_RANGES: [(&str, SettingRange); 5] = [
    (
        "general.checkIntervalMin",
        SettingRange { min: 1, max: 1440 },
    ),
    ("network.maxConnections", SettingRange { min: 1, max: 8 }),
    ("network.timeoutSec", SettingRange { min: 5, max: 3600 }),
    (
        "network.reconnectWaitMin",
        SettingRange { min: 1, max: 120 },
    ),
    ("cache.maxSizeGb", SettingRange { min: 1, max: 4096 }),
];

/// The range of a key of [`SETTING_RANGES`].
pub fn range_of(key: &str) -> Option<SettingRange> {
    SETTING_RANGES
        .iter()
        .find_map(|(k, r)| (*k == key).then_some(*r))
}

/// A stored value of a ranged key, clamped into its range (an older version may have allowed
/// more). `None` when `key` has no range or `value` is not a number.
pub fn clamp_stored(key: &str, value: &Value) -> Option<Value> {
    let range = range_of(key)?;
    let n = value.as_f64()?;
    if !n.is_finite() {
        return None;
    }
    let clamped = n.round().clamp(f64::from(range.min), f64::from(range.max));
    // In range of u32 after the clamp, so the cast is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(Value::from(clamped as u32))
}

fn invalid(key: &str, why: &str) -> AppError {
    crate::errors::invalid_argument(format!("Invalid value for setting {key}: {why}."))
        .with_detail(key)
}

fn check_range(key: &str, v: u32) -> Result<(), AppError> {
    let Some(r) = range_of(key) else {
        return Err(AppError::internal(format!("no range for setting {key}")));
    };
    if r.contains(v) {
        Ok(())
    } else {
        Err(invalid(
            key,
            &format!("must be between {} and {}", r.min, r.max),
        ))
    }
}

/// A link template is opened in the browser, so only http(s) may come out of it. Placeholders
/// such as `{server}` are replaced with a dummy host label before parsing.
fn check_link_template(key: &str, template: &str) -> Result<(), AppError> {
    let probe = template
        .replace("{server}", "server.example")
        .replace("{repo}", "repo")
        .replace("{rev}", "1")
        .replace("{revSpec}", "head")
        .replace("{path}", "path");
    let url = url::Url::parse(&probe).map_err(|_| invalid(key, "not a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid(key, "must start with http:// or https://"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid(key, "must not contain credentials"));
    }
    Ok(())
}

/// Proxy credentials live in the credential store, never in the settings file.
fn check_proxy(p: &ProxySetting) -> Result<(), AppError> {
    let ProxySetting::Custom { url } = p else {
        return Ok(());
    };
    let parsed = url::Url::parse(url).map_err(|_| invalid("network.proxy", "not a valid URL"))?;
    if !matches!(parsed.scheme(), "http" | "https" | "socks5") {
        return Err(invalid(
            "network.proxy",
            "scheme must be http, https or socks5",
        ));
    }
    if parsed.host_str().is_none() {
        return Err(invalid("network.proxy", "host is missing"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(invalid(
            "network.proxy",
            "credentials must not be part of the URL; they are stored in the credential store",
        ));
    }
    Ok(())
}

fn check_program(key: &str, tool: &ExternalDiffTool) -> Result<(), AppError> {
    if tool.path.trim().is_empty() || tool.path.contains('\0') {
        return Err(invalid(key, "program path is empty or invalid"));
    }
    if tool.args.iter().any(|a| a.contains('\0')) {
        return Err(invalid(key, "arguments contain an invalid character"));
    }
    Ok(())
}

/// Validates every field of `s`.
pub fn validate(s: &Settings) -> Result<(), AppError> {
    if s.general_checkout_root.trim().is_empty() || s.general_checkout_root.contains('\0') {
        return Err(invalid("general.checkoutRoot", "must not be empty"));
    }
    check_range("general.checkIntervalMin", s.general_check_interval_min)?;
    check_range(
        "network.maxConnections",
        u32::from(s.network_max_connections),
    )?;
    check_range("network.timeoutSec", s.network_timeout_sec)?;
    check_range("network.reconnectWaitMin", s.network_reconnect_wait_min)?;
    check_range("cache.maxSizeGb", s.cache_max_size_gb)?;
    if s.wc_default_format != WcFormat::V31 {
        return Err(invalid("wc.defaultFormat", "only format 31 can be written"));
    }
    check_proxy(&s.network_proxy)?;
    check_link_template(
        "integrations.webRevisionUrl",
        &s.integrations_web_revision_url,
    )?;
    check_link_template("integrations.webFileUrl", &s.integrations_web_file_url)?;
    for (key, tool) in [
        ("diff.externalTool", &s.diff_external_tool),
        ("diff.mergeTool", &s.diff_merge_tool),
        ("tools.editor", &s.tools_editor),
        ("tools.terminal", &s.tools_terminal),
    ] {
        if let Some(t) = tool {
            check_program(key, t)?;
        }
    }
    check_ext_tools(s.diff_tools_by_extension.as_deref().unwrap_or_default())
}

/// Most per-extension diff tools.
pub const MAX_EXT_TOOLS: usize = 32;

/// Whether `ext` is a stored extension: lowercase ASCII letters and digits, 1 to 16 of them (the
/// extensions the staged copies keep).
pub fn valid_ext(ext: &str) -> bool {
    (1..=16).contains(&ext.len())
        && ext
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn check_ext_tools(tools: &[ExtDiffTool]) -> Result<(), AppError> {
    const KEY: &str = "diff.toolsByExtension";
    if tools.len() > MAX_EXT_TOOLS {
        return Err(invalid(KEY, "too many extensions"));
    }
    for (i, t) in tools.iter().enumerate() {
        if !valid_ext(&t.ext) || tools[..i].iter().any(|o| o.ext == t.ext) {
            return Err(invalid(KEY, "extension is invalid or listed twice"));
        }
        check_program(
            KEY,
            &ExternalDiffTool {
                path: t.path.clone(),
                args: t.args.clone(),
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::settings::defaults::defaults;

    #[test]
    fn every_bound_is_enforced_and_inclusive() {
        for (key, r) in SETTING_RANGES {
            let set = |v: u32| {
                let mut s = defaults(None);
                match key {
                    "general.checkIntervalMin" => s.general_check_interval_min = v,
                    "network.maxConnections" => {
                        s.network_max_connections = u8::try_from(v).unwrap();
                    }
                    "network.timeoutSec" => s.network_timeout_sec = v,
                    "network.reconnectWaitMin" => s.network_reconnect_wait_min = v,
                    "cache.maxSizeGb" => s.cache_max_size_gb = v,
                    other => panic!("no field for ranged key {other}"),
                }
                validate(&s)
            };
            set(r.min).unwrap();
            set(r.max).unwrap();
            let low = set(r.min - 1).unwrap_err();
            assert_eq!(low.detail.as_deref(), Some(key), "{key}");
            set(r.max + 1).unwrap_err();
        }
    }

    #[test]
    fn the_connection_limit_matches_the_transport() {
        use svn_ra::transport::{DEFAULT_CONNECTIONS, MAX_CONNECTIONS, MIN_CONNECTIONS};
        let r = range_of("network.maxConnections").unwrap();
        assert_eq!(usize::try_from(r.min).unwrap(), MIN_CONNECTIONS);
        assert_eq!(usize::try_from(r.max).unwrap(), MAX_CONNECTIONS);
        assert_eq!(
            usize::from(defaults(None).network_max_connections),
            DEFAULT_CONNECTIONS
        );
    }

    #[test]
    fn stored_values_are_clamped_into_range() {
        use serde_json::json;
        assert_eq!(
            clamp_stored("network.maxConnections", &json!(16)),
            Some(json!(8))
        );
        assert_eq!(
            clamp_stored("network.timeoutSec", &json!(1)),
            Some(json!(5))
        );
        assert_eq!(
            clamp_stored("general.checkIntervalMin", &json!(0)),
            Some(json!(1))
        );
        assert_eq!(clamp_stored("cache.maxSizeGb", &json!(-3)), Some(json!(1)));
        assert_eq!(clamp_stored("network.proxy", &json!(3)), None);
        assert_eq!(clamp_stored("network.timeoutSec", &json!("x")), None);
    }

    #[test]
    fn defaults_are_valid() {
        validate(&defaults(None)).unwrap();
    }

    #[test]
    fn rejects_out_of_range_and_unsafe_values() {
        let ok = defaults(None);
        let mut s = ok.clone();
        s.network_max_connections = 9;
        assert!(validate(&s).is_err());
        let mut s = ok.clone();
        s.general_check_interval_min = 0;
        assert!(validate(&s).is_err());
        let mut s = ok.clone();
        s.wc_default_format = WcFormat::V32;
        assert!(validate(&s).is_err());
        let mut s = ok.clone();
        s.integrations_web_file_url = "javascript:alert(1)".into();
        assert!(validate(&s).is_err());
        let mut s = ok.clone();
        s.integrations_web_revision_url = "file:///etc/passwd".into();
        assert!(validate(&s).is_err());
        let mut s = ok.clone();
        s.network_proxy = ProxySetting::Custom {
            url: "http://user:pw@proxy:3128".into(),
        };
        assert!(validate(&s).is_err());
        let mut s = ok;
        s.network_proxy = ProxySetting::Custom {
            url: "http://proxy:3128".into(),
        };
        validate(&s).unwrap();
    }
}

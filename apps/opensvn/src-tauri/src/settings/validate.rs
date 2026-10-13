//! Semantic validation of a complete settings value.

use opensvn_ipc::{AppError, ExternalDiffTool, ProxySetting, Settings, WcFormat};

fn invalid(key: &str, why: &str) -> AppError {
    crate::errors::invalid_argument(format!("Invalid value for setting {key}: {why}."))
        .with_detail(key)
}

fn check_range(key: &str, v: u32, min: u32, max: u32) -> Result<(), AppError> {
    if (min..=max).contains(&v) {
        Ok(())
    } else {
        Err(invalid(key, &format!("must be between {min} and {max}")))
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
    check_range(
        "general.checkIntervalMin",
        s.general_check_interval_min,
        1,
        1440,
    )?;
    check_range(
        "network.maxConnections",
        u32::from(s.network_max_connections),
        1,
        8,
    )?;
    check_range("network.timeoutSec", s.network_timeout_sec, 5, 3600)?;
    check_range("cache.maxSizeGb", s.cache_max_size_gb, 1, 4096)?;
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
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::settings::defaults::defaults;

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

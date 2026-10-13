//! Configuration for `add`: `global-ignores`, `[auto-props]` and mime type detection.

use std::io::Read;
use std::path::Path;

use super::ignore::{DEFAULT_GLOBAL_IGNORES, fnmatch};

/// One `pattern = prop=value;prop2=value2` auto-props rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoPropRule {
    /// File name pattern (matched case-insensitively).
    pub pattern: String,
    /// Properties to set.
    pub props: Vec<(String, String)>,
}

/// The parts of the runtime configuration `svn add` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddConfig {
    /// `[miscellany] global-ignores`.
    pub global_ignores: Vec<String>,
    /// `[miscellany] enable-auto-props`.
    pub enable_auto_props: bool,
    /// `[auto-props]` rules in file order.
    pub auto_props: Vec<AutoPropRule>,
}

impl Default for AddConfig {
    fn default() -> Self {
        Self {
            global_ignores: DEFAULT_GLOBAL_IGNORES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            enable_auto_props: false,
            auto_props: Vec::new(),
        }
    }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "yes" | "true" | "on" | "1" => Some(true),
        "no" | "false" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Splits `prop=value;prop2=value2`; `;;` stands for a literal `;`. A property without `=`
/// gets an empty value.
pub fn parse_auto_prop_values(spec: &str) -> Vec<(String, String)> {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = spec.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ';' {
            if chars.peek() == Some(&';') {
                chars.next();
                cur.push(';');
            } else {
                parts.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    parts.push(cur);
    parts
        .into_iter()
        .filter_map(|p| {
            let p = p.trim().to_owned();
            if p.is_empty() {
                return None;
            }
            Some(match p.split_once('=') {
                Some((k, v)) => (k.trim().to_owned(), v.trim().to_owned()),
                None => (p, String::new()),
            })
        })
        .collect()
}

impl AddConfig {
    /// Parses the text of a Subversion `config` file; unknown sections and keys are ignored.
    pub fn from_config_text(text: &str) -> Self {
        let mut cfg = Self::default();
        let mut section = String::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.trim().to_ascii_lowercase();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match section.as_str() {
                "miscellany" if key == "global-ignores" => {
                    cfg.global_ignores = value.split_whitespace().map(str::to_owned).collect();
                }
                "miscellany" if key == "enable-auto-props" => {
                    cfg.enable_auto_props = parse_bool(value).unwrap_or(false);
                }
                "auto-props" => cfg.auto_props.push(AutoPropRule {
                    pattern: key.to_owned(),
                    props: parse_auto_prop_values(value),
                }),
                _ => {}
            }
        }
        cfg
    }

    /// Properties the configured rules assign to a file called `name`; later rules override
    /// earlier ones for the same property.
    pub fn props_for(&self, name: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        if !self.enable_auto_props {
            return out;
        }
        for rule in &self.auto_props {
            if fnmatch(&rule.pattern, name, true) {
                merge(&mut out, &rule.props);
            }
        }
        out
    }
}

/// Applies `src` over `dst`, replacing properties of the same name.
pub fn merge(dst: &mut Vec<(String, String)>, src: &[(String, String)]) {
    for (k, v) in src {
        dst.retain(|(dk, _)| dk != k);
        dst.push((k.clone(), v.clone()));
    }
}

/// Parses the value of an `svn:auto-props` directory property (`pattern = props` lines) and
/// returns the properties it assigns to `name`.
pub fn props_from_property(value: &str, name: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in value.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((pattern, props)) = line.split_once('=')
            && fnmatch(pattern.trim(), name, true)
        {
            merge(&mut out, &parse_auto_prop_values(props));
        }
    }
    out
}

/// `svn_io_detect_mimetype2` without a mime-types file: `application/octet-stream` for binary
/// content, `None` for text. Looks at the first 1024 bytes only.
pub fn detect_binary_mimetype(path: &Path) -> Option<&'static str> {
    let mut buf = [0u8; 1024];
    let mut file = std::fs::File::open(path).ok()?;
    let n = file.read(&mut buf).ok()?;
    let head = &buf[..n];
    if head.contains(&0) {
        return Some("application/octet-stream");
    }
    // Valid UTF-8 (a multibyte sequence cut at the end of the window is fine) is text.
    let valid = match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    };
    let control = head
        .iter()
        .filter(|b| **b < 0x20 && !matches!(**b, b'\t' | b'\n' | b'\r' | 0x0b | 0x0c | 0x08))
        .count();
    if !valid || control * 3 > n.max(1) {
        Some("application/octet-stream")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_sections() {
        let cfg = AddConfig::from_config_text(
            "[miscellany]\nglobal-ignores = *.bak build\nenable-auto-props = yes\n\
             [auto-props]\n*.txt = svn:eol-style=native;svn:keywords=Id\n*.sh = svn:executable\n",
        );
        assert_eq!(cfg.global_ignores, vec!["*.bak", "build"]);
        assert!(cfg.enable_auto_props);
        assert_eq!(
            cfg.props_for("A.TXT"),
            vec![
                ("svn:eol-style".into(), "native".into()),
                ("svn:keywords".into(), "Id".into())
            ]
        );
        assert_eq!(
            cfg.props_for("x.sh"),
            vec![("svn:executable".into(), String::new())]
        );
    }

    #[test]
    fn semicolon_escape() {
        assert_eq!(
            parse_auto_prop_values("a=b;;c;d=e"),
            vec![("a".into(), "b;c".into()), ("d".into(), "e".into())]
        );
    }
}

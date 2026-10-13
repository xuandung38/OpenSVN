//! Content of the diagnostics bundle (docs/system-design/02 section 3.4, minus wc.db details).

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use opensvn_ipc::Settings;

use super::DiagnosticsOptions;
use super::redact::{PrivacyFilter, redact_secrets};

/// Newest log text per file included in the bundle.
const LOG_TAIL_BYTES: usize = 200 * 1024;
/// Log files older than this are left out.
const LOG_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 3600);

/// Everything the bundle is built from.
pub struct BundleInputs {
    /// App version.
    pub app_version: String,
    /// `os arch` text.
    pub platform: String,
    /// Effective settings.
    pub settings: Settings,
    /// `(name, root path)` of the registered working copies.
    pub working_copies: Vec<(String, String)>,
    /// The user's home folder, if known.
    pub home_dir: Option<String>,
    /// Folder with rotating log files.
    pub logs_dir: Option<PathBuf>,
}

fn recent_log_tails(dir: &PathBuf) -> Vec<(String, String)> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    let mut files: Vec<(String, SystemTime, PathBuf)> = read
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata().ok()?;
            let modified = meta.modified().ok()?;
            (meta.is_file()
                && name.ends_with(".log")
                && now.duration_since(modified).ok()? <= LOG_MAX_AGE)
                .then_some((name, modified, e.path()))
        })
        .collect();
    files.sort_by_key(|(name, _, _)| name.clone());
    files
        .into_iter()
        .filter_map(|(name, _, path)| {
            let bytes = std::fs::read(path).ok()?;
            let start = bytes.len().saturating_sub(LOG_TAIL_BYTES);
            // Cut at a line start so the tail does not begin mid-line (or mid-character).
            let tail = &bytes[start..];
            let tail = match (start > 0, tail.iter().position(|b| *b == b'\n')) {
                (true, Some(i)) => &tail[i + 1..],
                _ => tail,
            };
            Some((
                format!("logs/{name}"),
                String::from_utf8_lossy(tail).into_owned(),
            ))
        })
        .collect()
}

/// Builds the bundle files as `(name, text)`. Secrets are always removed; paths and commit
/// messages only when requested.
pub fn build_bundle(inputs: &BundleInputs, options: DiagnosticsOptions) -> Vec<(String, String)> {
    let mut reps = Vec::new();
    if options.hide_paths {
        for (i, (_, root)) in inputs.working_copies.iter().enumerate() {
            reps.push((root.clone(), format!("<wc-{}>", i + 1)));
        }
        if let Some(home) = &inputs.home_dir {
            reps.push((home.clone(), "~".to_owned()));
        }
    }
    let filter = PrivacyFilter {
        path_replacements: reps,
        hide_commit_messages: options.hide_commit_messages,
    };
    let finish = |text: String| filter.apply(&redact_secrets(&text));

    let mut files = Vec::new();
    files.push((
        "app-info.txt".to_owned(),
        finish(format!(
            "OpenSVN {}\nplatform: {}\npaths hidden: {}\ncommit messages hidden: {}\n",
            inputs.app_version, inputs.platform, options.hide_paths, options.hide_commit_messages
        )),
    ));
    let settings = serde_json::to_string_pretty(&inputs.settings)
        .unwrap_or_else(|_| "settings unavailable".to_owned());
    files.push(("settings.json".to_owned(), finish(settings)));
    let wcs = inputs
        .working_copies
        .iter()
        .enumerate()
        .map(|(i, (name, root))| {
            if options.hide_paths {
                format!("<wc-{}>", i + 1)
            } else {
                format!("{name}\t{root}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    files.push(("working-copies.txt".to_owned(), finish(format!("{wcs}\n"))));
    if let Some(dir) = &inputs.logs_dir {
        for (name, text) in recent_log_tails(dir) {
            files.push((name, finish(text)));
        }
    }
    files
}

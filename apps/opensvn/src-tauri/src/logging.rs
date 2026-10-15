//! Log sinks. Every sink (stderr and `<data>/logs/opensvn.log`) is wrapped in svn-ra's
//! [`RedactingMakeWriter`], which scrubs URL userinfo and sensitive header values (authorization,
//! cookies, lock tokens) from each formatted record before it is written. [`redacted`] is the only
//! way a sink is built here, so a new sink cannot skip it.
//!
//! The diagnostics bundle reads the same `logs` folder and redacts again on export.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use opensvn_ipc::LogLevel;
use svn_ra::transport::RedactingMakeWriter;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{Registry, reload};

/// File name of the current log inside the logs folder.
pub const LOG_FILE: &str = "opensvn.log";
/// When the log grows past this size at startup it is moved to `opensvn.log.1`.
const ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// Wraps a sink so every record is redacted before it reaches it.
pub fn redacted<M>(make: M) -> RedactingMakeWriter<M> {
    RedactingMakeWriter::new(make)
}

/// The app's own crates: `general.logLevel` applies to them. Every other library logs at `info`
/// at most, so `debug` never turns on HTTP or TLS internals of dependencies.
const OWN_CRATES: [&str; 11] = [
    "opensvn_lib",
    "opensvn",
    "opensvn_ipc",
    "secure_store",
    "svn_client",
    "svn_delta",
    "svn_diff",
    "svn_protocol",
    "svn_ra",
    "svn_ra_api",
    "svn_wc",
];

fn level_filter(level: LogLevel) -> LevelFilter {
    match level {
        LogLevel::Error => LevelFilter::ERROR,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
    }
}

/// Filter for `level`: the app's crates at `level`, everything else at `level` or `info`,
/// whichever is less verbose.
pub fn targets(level: LogLevel) -> Targets {
    let own = level_filter(level);
    OWN_CRATES
        .iter()
        .fold(Targets::new(), |t, c| t.with_target(*c, own))
        .with_default(own.min(LevelFilter::INFO))
}

/// Changes the level of a running subscriber.
#[derive(Clone)]
pub struct LevelHandle(reload::Handle<Targets, Registry>);

impl LevelHandle {
    /// Applies `level` from now on.
    pub fn set(&self, level: LogLevel) {
        if let Err(e) = self.0.reload(targets(level)) {
            tracing::warn!(error = %e, "could not change the log level");
        }
    }
}

/// Handle of the global subscriber, once [`init`] installed it.
static GLOBAL_LEVEL: OnceLock<LevelHandle> = OnceLock::new();

/// Applies `general.logLevel` to the global subscriber (nothing before [`init`]).
pub fn set_level(level: LogLevel) {
    if let Some(h) = GLOBAL_LEVEL.get() {
        h.set(level);
    }
}

/// Subscriber writing to both sinks through [`redacted`], starting at `info`, plus the handle that
/// changes its level. Generic over the sinks so tests can capture what each one receives.
pub fn subscriber<A, B>(
    console: A,
    file: Option<B>,
) -> (impl tracing::Subscriber + Send + Sync, LevelHandle)
where
    A: for<'a> MakeWriter<'a> + Send + Sync + 'static,
    B: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let console = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(redacted(console));
    let file = file.map(|f| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(redacted(f))
    });
    let (filter, handle) = reload::Layer::new(targets(LogLevel::Info));
    let sub = tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .with(file);
    (sub, LevelHandle(handle))
}

/// Creates `logs` (owner-only on Unix), rotates an oversized log and opens it for appending.
pub fn open_log_file(logs_dir: &Path) -> io::Result<File> {
    create_private_dir(logs_dir)?;
    let path = logs_dir.join(LOG_FILE);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
        let mut old = PathBuf::from(&path);
        old.set_extension("log.1");
        std::fs::rename(&path, old)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Installs the global subscriber: stderr plus the log file in `logs_dir`. A log file that cannot
/// be opened leaves stderr only. Calling it again (tests) is a no-op.
pub fn init(logs_dir: &Path) {
    let file = match open_log_file(logs_dir) {
        Ok(f) => Some(Mutex::new(f)),
        Err(e) => {
            eprintln!("opensvn: cannot open the log file: {e}");
            None
        }
    };
    let (sub, handle) = subscriber(io::stderr, file);
    if tracing::subscriber::set_global_default(sub).is_ok() {
        let _ = GLOBAL_LEVEL.set(handle);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// In-memory sink shared with the test.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn every_sink_receives_redacted_records() {
        let console = Capture::default();
        let file = Capture::default();
        let (sub, _) = subscriber(console.clone(), Some(file.clone()));
        tracing::subscriber::with_default(sub, || {
            tracing::warn!(
                "request failed: url=https://alice:hunter2@svn.example.com/repo Authorization: Bearer ghp_topsecret"
            );
            tracing::info!(header = "cookie: session=abc123", "sent");
            tracing::debug!("below the level, never written");
        });
        for (name, sink) in [("console", &console), ("file", &file)] {
            let text = sink.text();
            assert!(text.contains("request failed"), "{name}: {text}");
            assert!(text.contains("svn.example.com"), "{name}: {text}");
            for secret in ["hunter2", "alice:", "ghp_topsecret", "abc123"] {
                assert!(!text.contains(secret), "{name} leaked {secret}: {text}");
            }
            assert!(!text.contains("below the level"), "{name}");
        }
    }

    #[test]
    fn the_level_changes_at_runtime_and_libraries_never_go_below_info() {
        let console = Capture::default();
        let (sub, level) = subscriber(console.clone(), None::<Capture>);
        tracing::subscriber::with_default(sub, || {
            tracing::debug!("own debug before");
            level.set(LogLevel::Debug);
            tracing::debug!("own debug after");
            tracing::debug!(target: "hyper::proto", "library debug");
            tracing::info!(target: "hyper::proto", "library info");
            level.set(LogLevel::Error);
            tracing::warn!("own warn at error level");
            tracing::info!(target: "hyper::proto", "library info at error level");
            tracing::error!("own error");
        });
        let text = console.text();
        assert!(!text.contains("own debug before"), "{text}");
        assert!(text.contains("own debug after"), "{text}");
        assert!(!text.contains("library debug"), "{text}");
        assert!(text.contains("library info"), "{text}");
        assert!(!text.contains("own warn at error level"), "{text}");
        assert!(!text.contains("library info at error level"), "{text}");
        assert!(text.contains("own error"), "{text}");
    }

    #[test]
    fn log_file_is_private_appended_and_rotated() {
        let dir = std::env::temp_dir().join(format!("opensvn-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let logs = dir.join("logs");
        {
            use std::io::Write;
            let mut f = open_log_file(&logs).unwrap();
            writeln!(f, "first").unwrap();
        }
        {
            use std::io::Write;
            let mut f = open_log_file(&logs).unwrap();
            writeln!(f, "second").unwrap();
        }
        let text = std::fs::read_to_string(logs.join(LOG_FILE)).unwrap();
        assert_eq!(text, "first\nsecond\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(&logs).unwrap().permissions().mode() & 0o777;
            let file_mode = std::fs::metadata(logs.join(LOG_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }
        std::fs::write(
            logs.join(LOG_FILE),
            vec![b'x'; usize::try_from(ROTATE_BYTES).unwrap() + 1],
        )
        .unwrap();
        drop(open_log_file(&logs).unwrap());
        assert_eq!(std::fs::metadata(logs.join(LOG_FILE)).unwrap().len(), 0);
        assert!(logs.join("opensvn.log.1").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_sink_is_built_without_redaction() {
        // Source guard: the only `with_writer` calls in this module go through `redacted`.
        let src = include_str!("logging.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        let writers = body.matches(".with_writer(").count();
        let redacted_writers = body.matches(".with_writer(redacted(").count();
        assert_eq!(writers, redacted_writers);
        assert!(writers >= 2);
        // ...and nothing else in the app installs a subscriber or a writer.
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        assert!(files.len() > 20);
        let needles = [
            [".with", "_writer("].concat(),
            ["tracing_subscriber::", "fmt()"].concat(),
            ["set_global", "_default"].concat(),
            ["fmt::", "init("].concat(),
        ];
        for f in files.iter().filter(|f| !f.ends_with("logging.rs")) {
            let text = std::fs::read_to_string(f).unwrap();
            for n in &needles {
                assert!(!text.contains(n.as_str()), "{} contains {n}", f.display());
            }
        }
    }
}

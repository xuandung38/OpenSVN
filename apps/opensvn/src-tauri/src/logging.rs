//! Log sinks. Every sink (stderr and `<data>/logs/opensvn.log`) is wrapped in svn-ra's
//! [`RedactingMakeWriter`], which scrubs URL userinfo and sensitive header values (authorization,
//! cookies, lock tokens) from each formatted record before it is written. [`redacted`] is the only
//! way a sink is built here, so a new sink cannot skip it.
//!
//! The diagnostics bundle reads the same `logs` folder and redacts again on export.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use svn_ra::transport::RedactingMakeWriter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;

/// File name of the current log inside the logs folder.
pub const LOG_FILE: &str = "opensvn.log";
/// When the log grows past this size at startup it is moved to `opensvn.log.1`.
const ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// Wraps a sink so every record is redacted before it reaches it.
pub fn redacted<M>(make: M) -> RedactingMakeWriter<M> {
    RedactingMakeWriter::new(make)
}

/// Subscriber writing to both sinks through [`redacted`]. Generic over the sinks so tests can
/// capture what each one receives.
pub fn subscriber<A, B>(console: A, file: Option<B>) -> impl tracing::Subscriber + Send + Sync
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
    tracing_subscriber::registry()
        .with(tracing_subscriber::filter::LevelFilter::INFO)
        .with(console)
        .with(file)
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

/// What Help > Show logs folder reveals: the current log, so the file manager opens the logs
/// folder with it selected, or the folder itself before anything was logged. The folder is created
/// (owner-only) when missing.
pub fn logs_reveal_target(logs_dir: &Path) -> io::Result<PathBuf> {
    create_private_dir(logs_dir)?;
    let file = logs_dir.join(LOG_FILE);
    Ok(if file.is_file() {
        file
    } else {
        logs_dir.to_path_buf()
    })
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
    let _ = tracing::subscriber::set_global_default(subscriber(io::stderr, file));
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
        let sub = subscriber(console.clone(), Some(file.clone()));
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

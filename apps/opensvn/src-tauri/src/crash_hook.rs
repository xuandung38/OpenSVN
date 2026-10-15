//! App-level crash tests (`apps/opensvn/e2e/crash.test.mjs`): stops the app at a named failpoint
//! so the test can kill it there. Compiled only with the `failpoints` feature, which no release
//! build enables.
//!
//! `OPENSVN_CRASH_FAILPOINT=<name>[:<hit>]` arms the failpoint `<name>` of `svn-wc`/`svn-client`.
//! On hit `<hit>` (1-based, default 1) the thread that reached it writes
//! `OPENSVN_CRASH_MARKER` (`<name> <hit> <pid>`) and then never returns, so the operation stays
//! exactly there until the test sends `SIGKILL`. Earlier hits pass through untouched.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const FAILPOINT_ENV: &str = "OPENSVN_CRASH_FAILPOINT";
const MARKER_ENV: &str = "OPENSVN_CRASH_MARKER";

/// Arms the failpoint named in the environment, if any.
pub fn install() {
    let (Ok(spec), Ok(marker)) = (std::env::var(FAILPOINT_ENV), std::env::var(MARKER_ENV)) else {
        return;
    };
    let (name, hit) = match spec.rsplit_once(':') {
        Some((name, hit)) => match hit.parse::<u64>() {
            Ok(hit) if hit > 0 => (name.to_owned(), hit),
            _ => {
                eprintln!("crash hook: bad hit number in {spec:?}, failpoint not armed");
                return;
            }
        },
        None => (spec.clone(), 1),
    };
    let marker = PathBuf::from(marker);
    let hits = AtomicU64::new(0);
    let point = name.clone();
    let armed = fail::cfg_callback(name.clone(), move || {
        if hits.fetch_add(1, Ordering::SeqCst) + 1 != hit {
            return;
        }
        let tmp = marker.with_extension("tmp");
        let line = format!("{point} {hit} {}\n", std::process::id());
        if std::fs::write(&tmp, line).is_ok() {
            let _ = std::fs::rename(&tmp, &marker);
        }
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    });
    // Logging starts later, in the app's setup: the driver log keeps stderr.
    match armed {
        Ok(()) => eprintln!("crash hook: {name} armed at hit {hit}"),
        Err(e) => eprintln!("crash hook: {name} not armed: {e}"),
    }
}

//! `compare-svn-log`: `log` through the OpenSVN engine against the installed svn CLI, on the
//! same throwaway Apache + `mod_dav_svn` (plain HTTP on `127.0.0.1`), over a repository with
//! `--revisions` revisions (100,000 by default).
//!
//! ```text
//! cargo run --release -p opensvn-bench --bin compare-svn-log -- [--revisions 100000]
//!     [--files 100] [--reps 3] [--out target/bench-report/log.md]
//! ```
//!
//! The repository is loaded from a generated dump (`svnadmin load`): r1 adds `--files` small
//! text files in 10 folders, every later revision changes one of them (round robin) with its
//! own log message. Measured:
//!
//! - `svn log -q URL`: every revision, no changed paths;
//! - `svn log -v URL`: every revision with its changed paths;
//! - `svn log -q -l 100 URL`: the newest 100;
//! - `svn log -q URL/file`: the history of one file (`revisions / files` entries).
//!
//! Every number is the wall time of a separate process (`svn log` with its output piped and
//! discarded, or `compare-svn-log engine log`, which counts what the server sent). The engine's
//! entry and changed-path counts must equal svn's, so a fast but incomplete log fails.

#[cfg(target_os = "linux")]
mod log_bench;
#[cfg(target_os = "linux")]
mod machine;

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(log_bench::main());
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("compare-svn-log runs on Linux only");
    std::process::exit(64);
}

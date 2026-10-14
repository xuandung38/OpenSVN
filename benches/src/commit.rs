//! `compare-svn-commit`: commit through the OpenSVN engine against the installed svn CLI, on the
//! same throwaway Apache + `mod_dav_svn` (plain HTTP on `127.0.0.1`), with the bytes each client
//! uploads counted by a pass-through TCP proxy in front of the server.
//!
//! ```text
//! cargo run --release -p opensvn-bench --bin compare-svn-commit -- [--files 10000]
//!     [--modify-files 1000] [--big-mb 40] [--reps 3] [--out target/bench-report/commit.md]
//! ```
//!
//! Measured, each client on its own repository and working copy:
//!
//! - **add**: `--files` new text files of about 1 to 3 KiB in 100 folders, scheduled with
//!   `svn add` (untimed), then one timed commit (the #121 commit case);
//! - **modify**: `--modify-files` of those files get one more line, then one timed commit;
//! - **big, 1 byte**: a `--big-mb` MiB random file is committed (untimed), then one byte in the
//!   middle changes and the timed commit follows (the #132 case).
//!
//! Every number is the wall time of a separate process (`svn commit` or `compare-svn-commit
//! engine commit`). After each engine commit an untimed `svn status` must be clean and
//! `svn cat` of a changed file must return the new content, so a fast but wrong commit fails.

#[cfg(target_os = "linux")]
mod commit_bench;

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(commit_bench::main());
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("compare-svn-commit runs on Linux only");
    std::process::exit(64);
}

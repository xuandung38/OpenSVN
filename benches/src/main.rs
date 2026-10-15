//! `compare-svn`: the OpenSVN engine against the installed svn CLI on the same local Apache
//! `mod_dav_svn`, on a generated repository (many text files plus large incompressible
//! binaries). Both clients run as separate processes timed from the outside (wall clock, process
//! start included), against the same server, from the same disk.
//!
//! ```text
//! cargo run --release -p opensvn-bench -- [--files 100000] [--binaries 4] [--binary-mb 64]
//!     [--update-files 10000] [--commit-files 1000] [--reps 3] [--out target/bench-report/bench.md]
//! ```
//!
//! Measured: checkout of HEAD; update r1 -> r2 (`--update-files` text files and one binary
//! changed); status of a clean working copy with all files, first run (after dropping the page
//! cache when allowed, otherwise right after checkout) and repeat runs; a commit of
//! `--commit-files` modified files. Every timed engine run is
//! checked afterwards with `svn status` (clean) so a fast but wrong run cannot pass.

#[cfg(target_os = "linux")]
mod bench;
#[cfg(target_os = "linux")]
mod machine;

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(bench::main());
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("compare-svn runs on Linux only");
    std::process::exit(64);
}

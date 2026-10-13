//! `cargo xtask` entry point. Currently hosts the `compat` command group.

mod compat;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("compat") => compat::run(&args[1..]),
        _ => {
            eprintln!("usage: cargo xtask compat <up|down|status|fixtures|run> [options]");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

//! `compat-run <scenario.yaml|dir>...`: run scenarios against the compose environment.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use opensvn_compat::runner::{DockerBackend, Runner};
use opensvn_compat::scenario::Scenario;
use opensvn_compat::{default_work_root, repo_root};

/// Collect `.yaml` files from files/directories (recursive), sorted for stable order.
fn collect(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    fn visit(p: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        if p.is_dir() {
            let mut kids: Vec<PathBuf> = std::fs::read_dir(p)
                .with_context(|| format!("read_dir {}", p.display()))?
                .map(|e| e.map(|e| e.path()))
                .collect::<std::io::Result<_>>()?;
            kids.sort();
            for k in kids {
                visit(&k, out)?;
            }
        } else if p.extension().is_some_and(|e| e == "yaml") {
            out.push(p.to_path_buf());
        }
        Ok(())
    }
    let mut out = Vec::new();
    for p in paths {
        visit(p, &mut out)?;
    }
    Ok(out)
}

fn run() -> Result<bool> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if args.is_empty() {
        bail!("usage: compat-run <scenario.yaml|dir>...");
    }
    let files = collect(&args)?;
    if files.is_empty() {
        bail!("no .yaml scenarios found");
    }
    let root = repo_root();
    let backend = DockerBackend::new(&root);
    let runner = Runner::new(&backend, default_work_root());
    let mut all_ok = true;
    for file in files {
        let scn = Scenario::load(&file)?;
        let report = runner
            .run(&scn)
            .with_context(|| format!("scenario {}", scn.name))?;
        for note in &report.manual_steps {
            println!("  manual step (not executed): {note}");
        }
        if report.passed() {
            println!("PASS {} ({} steps)", report.scenario, report.steps_run);
        } else {
            all_ok = false;
            println!("FAIL {}", report.scenario);
            for m in &report.mismatches {
                println!("  {m}");
            }
        }
    }
    Ok(all_ok)
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

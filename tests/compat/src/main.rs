//! `compat-run <scenario.yaml|dir>...`: run scenarios against the compose environment.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use opensvn_compat::local::LocalBackend;
use opensvn_compat::runner::{Backend, DockerBackend, Runner};
use opensvn_compat::scenario::Scenario;
use opensvn_compat::{default_work_root, repo_root};

/// Collect `.yaml` files from files/directories (recursive), sorted for stable order.
fn collect(paths: &[PathBuf], include_legacy: bool) -> Result<Vec<PathBuf>> {
    fn visit(p: &Path, out: &mut Vec<PathBuf>, include_legacy: bool) -> Result<()> {
        // `legacy/` scenarios need the 1.8-1.10 containers (`up --legacy`); opt-in.
        if p.is_dir() && p.file_name().is_some_and(|n| n == "legacy") && !include_legacy {
            return Ok(());
        }
        if p.is_dir() {
            let mut kids: Vec<PathBuf> = std::fs::read_dir(p)
                .with_context(|| format!("read_dir {}", p.display()))?
                .map(|e| e.map(|e| e.path()))
                .collect::<std::io::Result<_>>()?;
            kids.sort();
            for k in kids {
                visit(&k, out, include_legacy)?;
            }
        } else if p.extension().is_some_and(|e| e == "yaml") {
            out.push(p.to_path_buf());
        }
        Ok(())
    }
    let mut out = Vec::new();
    for p in paths {
        visit(p, &mut out, include_legacy)?;
    }
    Ok(out)
}

fn run() -> Result<bool> {
    let mut args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let include_legacy = args.iter().any(|a| a == "--legacy");
    args.retain(|a| a != "--legacy");
    if args.is_empty() {
        bail!("usage: compat-run [--legacy] <scenario.yaml|dir>...");
    }
    let files = collect(&args, include_legacy)?;
    if files.is_empty() {
        bail!("no .yaml scenarios found");
    }
    let root = repo_root();
    // `OPENSVN_COMPAT_BACKEND=local`: servers and clients on the host (see README).
    let local = std::env::var("OPENSVN_COMPAT_BACKEND").is_ok_and(|b| b == "local");
    let backend: Box<dyn Backend> = if local {
        Box::new(LocalBackend::from_env(&default_work_root().join(".svn-config"))?)
    } else {
        Box::new(DockerBackend::new(&root))
    };
    let runner = Runner::new(backend.as_ref(), default_work_root());
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

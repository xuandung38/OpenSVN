//! `cargo xtask compat ...`: manage the Dockerised svn 1.14/1.15 compat environment.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};

const PROJECT: &str = "opensvn-compat";
const USAGE: &str = "\
cargo xtask compat <command>

  up [--no-build]   build images (unless --no-build), start both servers, wait for healthy
  down [--purge]    stop and remove containers and volumes (--purge also deletes target/compat)
  status            show container state
  fixtures          create the fixture repos (basic, nolayout) on both servers and verify hashes
                    (--update-hash records new expected hashes; --large [N] adds the perf repo)
  run [paths...]    run scenario files/dirs (default: tests/compat/scenarios/harness)";

/// Repo root = parent of this crate's directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .components()
        .collect()
}

fn state_dir(root: &Path) -> PathBuf {
    root.join("target").join("compat")
}

/// `docker compose` with project, file and host uid/gid so client containers write host-owned files.
fn compose(root: &Path) -> Command {
    let mut cmd = Command::new("docker");
    cmd.args(["compose", "-f"])
        .arg(root.join("docker").join("compose.yml"))
        .args(["-p", PROJECT]);
    if let Some(uid) = id_of("-u") {
        cmd.env("HOST_UID", uid);
    }
    if let Some(gid) = id_of("-g") {
        cmd.env("HOST_GID", gid);
    }
    cmd
}

fn id_of(flag: &str) -> Option<String> {
    let out = Command::new("id").arg(flag).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn exec(cmd: &mut Command, what: &str) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("{what}: failed to spawn docker"))?;
    if !status.success() {
        bail!("{what} failed ({status})");
    }
    Ok(())
}

/// Dispatch a `compat` subcommand.
pub fn run(args: &[String]) -> Result<()> {
    let root = repo_root();
    let rest = args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str) {
        Some("up") => up(&root, rest),
        Some("down") => down(&root, rest),
        Some("status") => exec(compose(&root).arg("ps").arg("-a"), "compose ps"),
        Some("fixtures") => fixtures(&root, rest),
        Some("run") => run_scenarios(&root, rest),
        _ => {
            println!("{USAGE}");
            bail!("unknown or missing compat command")
        }
    }
}

fn up(root: &Path, args: &[String]) -> Result<()> {
    let state = state_dir(root);
    for sub in ["certs", "work", "logs/svn14", "logs/svn15"] {
        fs::create_dir_all(state.join(sub)).with_context(|| format!("create {sub}"))?;
    }
    let started = Instant::now();
    let mut cmd = compose(root);
    cmd.args([
        "up",
        "-d",
        "--wait",
        "--wait-timeout",
        "120",
        "svn14-server",
        "svn15-server",
        "svn14-client",
        "svn15-client",
    ]);
    if !args.iter().any(|a| a == "--no-build") {
        cmd.arg("--build");
    }
    exec(&mut cmd, "compose up")?;
    println!(
        "compat environment ready in {:.1}s",
        started.elapsed().as_secs_f32()
    );
    println!(
        "  svn 1.14.5 server: https://localhost:8443/svn/  (svn14.test.local inside the network)"
    );
    println!(
        "  svn 1.15.0 server: https://localhost:8444/svn/  (svn15.test.local inside the network)"
    );
    println!("  test CA: {}", state.join("certs/ca.crt").display());
    Ok(())
}

fn down(root: &Path, args: &[String]) -> Result<()> {
    exec(
        compose(root).args(["down", "-v", "--remove-orphans"]),
        "compose down",
    )?;
    if args.iter().any(|a| a == "--purge") {
        let state = state_dir(root);
        if state.exists() {
            fs::remove_dir_all(&state).with_context(|| format!("remove {}", state.display()))?;
        }
    }
    Ok(())
}

fn fixtures(root: &Path, args: &[String]) -> Result<()> {
    let update = args.iter().any(|a| a == "--update-hash");
    let large = args.iter().position(|a| a == "--large").map(|i| {
        args.get(i + 1)
            .filter(|v| !v.starts_with("--"))
            .cloned()
            .unwrap_or_else(|| "100000".into())
    });
    for server in ["svn14-server", "svn15-server"] {
        let mut script = String::from(
            "set -e; for r in basic nolayout; do /fixtures/repos/make-$r.sh /var/svn/repos/$r; done;",
        );
        if let Some(n) = &large {
            if n.parse::<u32>().is_err() {
                bail!("--large expects a number, got {n}");
            }
            script.push_str(&format!(
                " /fixtures/repos/make-large.sh /var/svn/repos/large {n};"
            ));
        }
        script.push_str(" chown -R www-data:www-data /var/svn/repos");
        let mut cmd = compose(root);
        cmd.args(["exec", "-T", "-u", "root"]);
        if update {
            cmd.args(["-e", "FIXTURE_UPDATE_HASH=1"]);
        }
        cmd.args([server, "bash", "-c", &script]);
        println!("== fixtures on {server}");
        exec(&mut cmd, "fixture creation")?;
    }
    Ok(())
}

fn run_scenarios(root: &Path, args: &[String]) -> Result<()> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(root).args([
        "run",
        "--quiet",
        "--locked",
        "-p",
        "opensvn-compat",
        "--bin",
        "compat-run",
        "--",
    ]);
    if args.is_empty() {
        cmd.arg(root.join("tests/compat/scenarios/harness"));
    } else {
        cmd.args(args);
    }
    exec(&mut cmd, "compat-run")
}

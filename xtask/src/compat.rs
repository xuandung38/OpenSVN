//! `cargo xtask compat ...`: manage the Dockerised svn 1.14/1.15 compat environment.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};

const PROJECT: &str = "opensvn-compat";
/// Compose service prefixes always started by `up`.
const SERVER_TAGS: [&str; 2] = ["svn14", "svn15"];
/// Extra prefixes started with `up --legacy` (Subversion 1.10.8, 1.9.12, 1.8.19).
const LEGACY_TAGS: [&str; 3] = ["svn110", "svn19", "svn18"];
const USAGE: &str = "\
cargo xtask compat <command>

  up [--no-build] [--legacy]
                    build images (unless --no-build), start the 1.14.5/1.15.0 servers and wait for
                    healthy; --legacy also starts the 1.10.8, 1.9.12 and 1.8.19 containers
  down [--purge]    stop and remove containers and volumes (--purge also deletes target/compat)
  status            show container state
  fixtures [--legacy]
                    create the fixture repos (basic, nolayout) on both servers and verify hashes
                    (--update-hash records new expected hashes; --large [N] adds the perf repo)
  run [--legacy] [paths...]
                    run scenario files/dirs (default: tests/compat/scenarios/harness; --legacy
                    includes the harness/legacy smoke scenarios for 1.8-1.10)";

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
        .args(["--profile", "legacy", "-p", PROJECT]);
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
    let legacy = args.iter().any(|a| a == "--legacy");
    for sub in ["certs", "work"] {
        fs::create_dir_all(state.join(sub)).with_context(|| format!("create {sub}"))?;
    }
    for tag in SERVER_TAGS
        .iter()
        .chain(legacy.then_some(&LEGACY_TAGS).into_iter().flatten())
    {
        fs::create_dir_all(state.join("logs").join(tag))
            .with_context(|| format!("create logs/{tag}"))?;
    }
    let started = Instant::now();
    let mut cmd = compose(root);
    cmd.args(["up", "-d", "--wait", "--wait-timeout", "120"]);
    for tag in SERVER_TAGS
        .iter()
        .chain(legacy.then_some(&LEGACY_TAGS).into_iter().flatten())
    {
        cmd.args([format!("{tag}-server"), format!("{tag}-client")]);
    }
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
    let legacy = args.iter().any(|a| a == "--legacy");
    let servers = SERVER_TAGS
        .iter()
        .chain(legacy.then_some(&LEGACY_TAGS).into_iter().flatten())
        .map(|t| format!("{t}-server"));
    for server in servers {
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
        cmd.args([server.as_str(), "bash", "-c", &script]);
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
    cmd.args(args);
    // Only flags given: run the default harness directory.
    if args.iter().all(|a| a.starts_with("--")) {
        cmd.arg(root.join("tests/compat/scenarios/harness"));
    }
    exec(&mut cmd, "compat-run")
}

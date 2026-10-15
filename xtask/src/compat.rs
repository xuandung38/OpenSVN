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
  fixtures [--repos] [--wc] [--protocol] [--hostile] [--svn <path>] [--legacy]
                    run the fixture generators; with no selection flag, all of them:
                    --repos     create the fixture repos (basic, nolayout) on the running servers
                                and verify hashes (--update-hash records new expected hashes;
                                --large [N] adds the perf repo)
                    --wc        regenerate the WC state tarballs (fixtures/wc/make-wc-fixtures.sh):
                                format 31, plus format 32 when the client is svn 1.15+
                    --protocol  re-capture the protocol transcripts (fixtures/protocol/capture.sh;
                                native Apache + mod_dav_svn)
                    --hostile   build the hostile repository (fixtures/evil/make-hostile-repo.sh)
                                into target/compat/hostile
                    --svn <path>  svn client for --wc/--protocol/--hostile (default: svn on PATH;
                                its directory is put first on PATH so svnadmin etc. match)
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

/// Which generators `compat fixtures` runs.
#[derive(Debug, PartialEq, Eq)]
struct FixtureSteps {
    repos: bool,
    wc: bool,
    protocol: bool,
    hostile: bool,
}

impl FixtureSteps {
    /// No selection flag means every generator.
    fn from_args(args: &[String]) -> Self {
        let has = |f: &str| args.iter().any(|a| a == f);
        let any = ["--repos", "--wc", "--protocol", "--hostile"]
            .iter()
            .any(|f| has(f));
        Self {
            repos: !any || has("--repos"),
            wc: !any || has("--wc"),
            protocol: !any || has("--protocol"),
            hostile: !any || has("--hostile"),
        }
    }
}

/// Value after `flag` (`--svn <path>`).
fn flag_value<'a>(args: &'a [String], flag: &str) -> Result<Option<&'a str>> {
    match args.iter().position(|a| a == flag) {
        None => Ok(None),
        Some(i) => match args.get(i + 1) {
            Some(v) if !v.starts_with("--") => Ok(Some(v.as_str())),
            _ => bail!("{flag} expects a value"),
        },
    }
}

fn fixtures(root: &Path, args: &[String]) -> Result<()> {
    let steps = FixtureSteps::from_args(args);
    let svn = flag_value(args, "--svn")?;
    if steps.wc {
        wc_fixtures(root, svn)?;
    }
    if steps.protocol {
        println!("== protocol captures");
        exec(
            native(root, svn).arg(root.join("fixtures/protocol/capture.sh")),
            "protocol capture",
        )?;
    }
    if steps.hostile {
        let out = state_dir(root).join("hostile");
        println!("== hostile repository -> {}", out.display());
        exec(
            native(root, svn)
                .arg(root.join("fixtures/evil/make-hostile-repo.sh"))
                .arg(out.join("repo")),
            "hostile repository",
        )?;
    }
    if steps.repos {
        server_repos(root, args)?;
    }
    Ok(())
}

/// `bash <script>` from the repository root, with the directory of `svn` (if given) first on
/// PATH so the generators use its svnadmin/svnlook/svnmucc/svnserve too.
fn native(root: &Path, svn: Option<&str>) -> Command {
    let mut cmd = Command::new("bash");
    cmd.current_dir(root);
    if let Some(svn) = svn {
        let svn = Path::new(svn);
        if let Some(dir) = svn.parent().filter(|d| !d.as_os_str().is_empty()) {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut dirs = vec![dir.to_path_buf()];
            dirs.extend(std::env::split_paths(&path));
            if let Ok(joined) = std::env::join_paths(dirs) {
                cmd.env("PATH", joined);
            }
        }
        cmd.env("SVN", svn);
    }
    cmd
}

fn wc_fixtures(root: &Path, svn: Option<&str>) -> Result<()> {
    let version = Command::new(svn.unwrap_or("svn"))
        .args(["--version", "--quiet"])
        .output()
        .context("wc fixtures: failed to run svn --version")?;
    let version = String::from_utf8_lossy(&version.stdout).trim().to_string();
    let script = root.join("fixtures/wc/make-wc-fixtures.sh");
    let mut formats = vec!["31"];
    if wc_format_32_supported(&version) {
        formats.push("32");
    }
    for fmt in formats {
        println!("== WC fixtures, format {fmt} (svn {version})");
        exec(native(root, svn).arg(&script).arg(fmt), "wc fixtures")?;
    }
    Ok(())
}

/// Format 32 (`--compatible-version 1.15`) exists from svn 1.15 on.
fn wc_format_32_supported(version: &str) -> bool {
    let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (major, minor) >= (1, 15)
}

fn server_repos(root: &Path, args: &[String]) -> Result<()> {
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn no_selection_runs_every_generator() {
        let all = FixtureSteps {
            repos: true,
            wc: true,
            protocol: true,
            hostile: true,
        };
        assert_eq!(FixtureSteps::from_args(&[]), all);
        assert_eq!(FixtureSteps::from_args(&args(&["--legacy"])), all);
    }

    #[test]
    fn selection_flags_pick_generators() {
        assert_eq!(
            FixtureSteps::from_args(&args(&["--wc", "--protocol"])),
            FixtureSteps {
                repos: false,
                wc: true,
                protocol: true,
                hostile: false,
            }
        );
        assert_eq!(
            FixtureSteps::from_args(&args(&["--repos", "--large", "10"])),
            FixtureSteps {
                repos: true,
                wc: false,
                protocol: false,
                hostile: false,
            }
        );
    }

    #[test]
    fn svn_flag_needs_a_value() {
        assert_eq!(
            flag_value(&args(&["--svn", "/x/svn"]), "--svn").unwrap(),
            Some("/x/svn")
        );
        assert!(flag_value(&args(&["--svn"]), "--svn").is_err());
        assert!(flag_value(&args(&["--svn", "--wc"]), "--svn").is_err());
        assert_eq!(flag_value(&args(&["--wc"]), "--svn").unwrap(), None);
    }

    #[test]
    fn format_32_needs_svn_1_15() {
        assert!(wc_format_32_supported("1.15.0"));
        assert!(wc_format_32_supported("1.16.1"));
        assert!(!wc_format_32_supported("1.14.3"));
        assert!(!wc_format_32_supported("1.8.19"));
        assert!(!wc_format_32_supported(""));
    }
}

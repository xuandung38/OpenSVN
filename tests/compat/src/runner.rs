//! Scenario runner: executes steps through a [`Backend`] (docker compose exec by default) and
//! evaluates the comparisons.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use crate::compare::{diff_lines_unordered, diff_text, diff_trees, normalize_xml, snapshot_tree};
use crate::scenario::{Actor, Check, Compare, Expect, Scenario, Step, SvnVersion};
use crate::wcdb_dump::{dump_wc_db, work_queue_rows};

/// Test-only Basic credentials baked into the compat server image.
pub const TEST_USER: &str = "tester";
/// Test-only password (see `docker/svn-server/Dockerfile`).
pub const TEST_PASSWORD: &str = "tester-pass";
const COMPOSE_PROJECT: &str = "opensvn-compat";
/// Mount point of the shared work dir inside client containers.
const CONTAINER_WORK: &str = "/work";

/// Captured result of a command.
#[derive(Debug, Clone)]
pub struct CmdOutput {
    /// Exit status code (-1 when killed by a signal).
    pub code: i32,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
}

/// Where a command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Client container of the given version.
    Client(SvnVersion),
    /// Server container of the given version, as root.
    Server(SvnVersion),
}

/// Command execution abstraction so the runner is testable without Docker.
pub trait Backend {
    /// Run `argv` on `target` with working directory `cwd` (container path).
    fn exec(&self, target: Target, cwd: &str, argv: &[String]) -> Result<CmdOutput>;
}

/// Drives the OpenSVN engine for `on: engine` steps.
pub trait EngineDriver {
    /// Execute an `svn`-style step against the working copy directory on the host.
    fn run(&self, step: &Step, wc_dir: &Path, svn_args: &[String]) -> Result<()>;
}

/// `docker compose exec` backend against the `opensvn-compat` project.
#[derive(Debug, Clone)]
pub struct DockerBackend {
    compose_file: PathBuf,
}

impl DockerBackend {
    /// Backend using `<repo_root>/docker/compose.yml`.
    pub fn new(repo_root: &Path) -> Self {
        Self {
            compose_file: repo_root.join("docker").join("compose.yml"),
        }
    }
}

impl Backend for DockerBackend {
    fn exec(&self, target: Target, cwd: &str, argv: &[String]) -> Result<CmdOutput> {
        let (service, user) = match target {
            Target::Client(v) => (format!("{}-client", v.tag()), None),
            Target::Server(v) => (format!("{}-server", v.tag()), Some("root")),
        };
        let mut cmd = Command::new("docker");
        cmd.args(["compose", "-f"]).arg(&self.compose_file).args([
            "-p",
            COMPOSE_PROJECT,
            "exec",
            "-T",
            "-w",
            cwd,
        ]);
        if let Some(u) = user {
            cmd.args(["-u", u]);
        }
        cmd.arg(service).args(argv);
        let out = cmd
            .output()
            .context("failed to spawn docker (is Docker running?)")?;
        Ok(CmdOutput {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// Outcome of running a scenario.
#[derive(Debug, Default)]
pub struct Report {
    /// Scenario name.
    pub scenario: String,
    /// Steps executed.
    pub steps_run: usize,
    /// Human-performed steps that were only recorded.
    pub manual_steps: Vec<String>,
    /// Comparison mismatches (empty means the scenario passed).
    pub mismatches: Vec<String>,
}

impl Report {
    /// True when no comparison mismatched.
    pub fn passed(&self) -> bool {
        self.mismatches.is_empty()
    }
}

/// Executes scenarios.
pub struct Runner<'a> {
    backend: &'a dyn Backend,
    engine: Option<&'a dyn EngineDriver>,
    work_root: PathBuf,
}

impl<'a> Runner<'a> {
    /// `work_root` is the host directory bind-mounted at `/work` in client containers.
    pub fn new(backend: &'a dyn Backend, work_root: PathBuf) -> Self {
        Self {
            backend,
            engine: None,
            work_root,
        }
    }

    /// Attach an engine driver for `on: engine` steps.
    #[must_use]
    pub fn with_engine(mut self, engine: &'a dyn EngineDriver) -> Self {
        self.engine = Some(engine);
        self
    }

    /// Run all steps then all comparisons. Harness failures are `Err`; behavioural
    /// differences are reported in [`Report::mismatches`].
    pub fn run(&self, scn: &Scenario) -> Result<Report> {
        let host_dir = self.work_root.join(&scn.name);
        if host_dir.exists() {
            fs::remove_dir_all(&host_dir)
                .with_context(|| format!("clean {}", host_dir.display()))?;
        }
        fs::create_dir_all(&host_dir)?;

        let repo_name = if scn.fresh_repo {
            let name = format!("{}-{}", scn.name, std::process::id());
            self.hotcopy(scn.server, &scn.repo, &name)?;
            name
        } else {
            scn.repo.clone()
        };
        let result = self.run_inner(scn, &repo_name, &host_dir);
        if scn.fresh_repo {
            // Best effort: a leftover copy only wastes volume space.
            let _ = self.server_sh(scn.server, &format!("rm -rf /var/svn/repos/'{repo_name}'"));
        }
        result
    }

    fn run_inner(&self, scn: &Scenario, repo_name: &str, host_dir: &Path) -> Result<Report> {
        let url = format!(
            "https://{}.test.local:8443/svn/{repo_name}",
            scn.server.tag()
        );
        let mut report = Report {
            scenario: scn.name.clone(),
            ..Report::default()
        };
        for (i, step) in scn.steps.iter().enumerate() {
            let wc_host = host_dir.join(&step.wc);
            fs::create_dir_all(&wc_host)?;
            let wc_container = format!("{CONTAINER_WORK}/{}/{}", scn.name, step.wc);
            let subst = |s: &str| {
                s.replace("{url}", &url)
                    .replace("{repo}", repo_name)
                    .replace("{wc}", &wc_container)
            };
            match step.on {
                Actor::TortoiseManual => {
                    report
                        .manual_steps
                        .push(step.note.clone().unwrap_or_default());
                    continue;
                }
                Actor::Engine => {
                    let driver = self.engine.ok_or_else(|| {
                        anyhow!("step {i}: engine step but no EngineDriver configured")
                    })?;
                    let args: Vec<String> = step.svn.iter().flatten().map(|a| subst(a)).collect();
                    driver
                        .run(step, &wc_host, &args)
                        .with_context(|| format!("step {i} (engine)"))?;
                }
                Actor::Svn14 | Actor::Svn15 => {
                    let version = if step.on == Actor::Svn14 {
                        SvnVersion::V14
                    } else {
                        SvnVersion::V15
                    };
                    let argv = if let Some(svn) = &step.svn {
                        let mut v = vec!["svn".to_string()];
                        v.extend(svn.iter().map(|a| subst(a)));
                        v.extend(auth_args());
                        v
                    } else if let Some(sh) = &step.sh {
                        vec!["sh".into(), "-c".into(), subst(sh)]
                    } else {
                        bail!("step {i}: nothing to execute");
                    };
                    let out = self
                        .backend
                        .exec(Target::Client(version), &wc_container, &argv)?;
                    if out.code != 0 {
                        bail!(
                            "step {i} ({:?} {}) exited {}\nstdout: {}\nstderr: {}",
                            step.on,
                            argv.join(" "),
                            out.code,
                            out.stdout,
                            out.stderr
                        );
                    }
                }
            }
            report.steps_run += 1;
        }
        for cmp in &scn.compare {
            self.compare(scn, cmp, host_dir, &mut report)?;
        }
        Ok(report)
    }

    fn compare(
        &self,
        scn: &Scenario,
        cmp: &Compare,
        host_dir: &Path,
        report: &mut Report,
    ) -> Result<()> {
        let reader = cmp.reader;
        for check in &cmp.checks {
            let diffs = match check {
                Check::Tree => diff_trees(
                    &snapshot_tree(&host_dir.join(&cmp.left))?,
                    &snapshot_tree(&host_dir.join(&cmp.right))?,
                ),
                Check::Status | Check::Info => {
                    let sub: &[&str] = if *check == Check::Status {
                        &["status", "--xml", "--verbose"]
                    } else {
                        &["info", "--xml", "--recursive"]
                    };
                    let l = self.wc_xml(scn, reader, &cmp.left, sub)?;
                    let r = self.wc_xml(scn, reader, &cmp.right, sub)?;
                    diff_text(&l, &r)
                }
                Check::Wcdb => diff_lines_unordered(
                    &dump_wc_db(&host_dir.join(&cmp.left))?,
                    &dump_wc_db(&host_dir.join(&cmp.right))?,
                ),
            };
            let mut diffs = diffs;
            if *check == Check::Wcdb {
                // A finished operation leaves no queued work: non-empty means the WC needs cleanup.
                for side in [&cmp.left, &cmp.right] {
                    let n = work_queue_rows(&host_dir.join(side))?;
                    if n > 0 {
                        diffs.push(format!("WORK_QUEUE of {side} has {n} pending row(s)"));
                    }
                }
            }
            if scn.expect == Expect::Mismatch {
                // Negative control: the check must notice the planted difference.
                if diffs.is_empty() {
                    report.mismatches.push(format!(
                        "[{}] {:?} {} vs {}: expected a difference but found none (comparator too lax?)",
                        scn.name, check, cmp.left, cmp.right
                    ));
                }
                continue;
            }
            for d in diffs {
                report.mismatches.push(format!(
                    "[{}] {:?} {} vs {}: {d}",
                    scn.name, check, cmp.left, cmp.right
                ));
            }
        }
        Ok(())
    }

    /// Run `svn <sub> ...` in a working copy and return normalised XML.
    fn wc_xml(
        &self,
        scn: &Scenario,
        version: SvnVersion,
        wc: &str,
        sub: &[&str],
    ) -> Result<String> {
        let cwd = format!("{CONTAINER_WORK}/{}/{wc}", scn.name);
        let mut argv: Vec<String> = vec!["svn".into()];
        argv.extend(sub.iter().map(ToString::to_string));
        argv.extend(auth_args());
        argv.push(".".into());
        let out = self.backend.exec(Target::Client(version), &cwd, &argv)?;
        if out.code != 0 {
            bail!("{} failed in {wc}: {}", argv.join(" "), out.stderr);
        }
        Ok(normalize_xml(&out.stdout, &cwd))
    }

    fn hotcopy(&self, server: SvnVersion, from: &str, to: &str) -> Result<()> {
        self.server_sh(
            server,
            &format!(
                "svnadmin hotcopy /var/svn/repos/'{from}' /var/svn/repos/'{to}' && chown -R www-data:www-data /var/svn/repos/'{to}'"
            ),
        )
        .with_context(|| format!("hotcopy fixture repo '{from}' (did `cargo xtask compat fixtures` run?)"))
    }

    fn server_sh(&self, server: SvnVersion, script: &str) -> Result<()> {
        let argv = vec!["sh".to_string(), "-c".into(), script.to_string()];
        let out = self.backend.exec(Target::Server(server), "/", &argv)?;
        if out.code != 0 {
            bail!("server command failed ({}): {}", out.code, out.stderr);
        }
        Ok(())
    }
}

fn auth_args() -> Vec<String> {
    [
        "--username",
        TEST_USER,
        "--password",
        TEST_PASSWORD,
        "--non-interactive",
    ]
    .iter()
    .map(ToString::to_string)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records commands and succeeds; `svn status/info` returns fixed XML.
    struct FakeBackend {
        calls: RefCell<Vec<(Target, String, Vec<String>)>>,
    }

    impl Backend for FakeBackend {
        fn exec(&self, target: Target, cwd: &str, argv: &[String]) -> Result<CmdOutput> {
            self.calls
                .borrow_mut()
                .push((target, cwd.to_string(), argv.to_vec()));
            let stdout = if argv.iter().any(|a| a == "--xml") {
                format!("<x><path>{cwd}</path></x>")
            } else {
                String::new()
            };
            Ok(CmdOutput {
                code: 0,
                stdout,
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn substitutes_placeholders_and_compares_equal_trees() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let scn = Scenario::from_yaml(
            "name: t1\nrepo: basic\nsteps:\n  - { on: svn15, wc: a, svn: [checkout, '{url}/trunk', '.'] }\n  - { on: svn15, wc: b, sh: 'echo {repo}' }\ncompare:\n  - { left: a, right: b, checks: [tree, status] }\n",
        )?;
        let backend = FakeBackend {
            calls: RefCell::new(Vec::new()),
        };
        let report = Runner::new(&backend, tmp.path().to_path_buf()).run(&scn)?;
        assert!(report.passed(), "{:?}", report.mismatches);
        let calls = backend.calls.borrow();
        assert_eq!(
            calls[0].2[2],
            "https://svn15.test.local:8443/svn/basic/trunk"
        );
        assert!(calls[0].2.contains(&"--non-interactive".to_string()));
        assert_eq!(calls[1].2[2], "echo basic");
        Ok(())
    }

    #[test]
    fn fresh_repo_hotcopies_and_cleans_up() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let scn = Scenario::from_yaml(
            "name: t2\nrepo: basic\nfresh_repo: true\nsteps:\n  - { on: svn14, wc: a, svn: [info, '{url}'] }\n",
        )?;
        let backend = FakeBackend {
            calls: RefCell::new(Vec::new()),
        };
        Runner::new(&backend, tmp.path().to_path_buf()).run(&scn)?;
        let calls = backend.calls.borrow();
        assert!(calls[0].2[2].contains("hotcopy"));
        assert!(matches!(calls[0].0, Target::Server(SvnVersion::V15)));
        assert!(calls.last().is_some_and(|c| c.2[2].contains("rm -rf")));
        assert!(
            calls[1].2[2].contains("t2-"),
            "url uses the copy: {:?}",
            calls[1].2
        );
        Ok(())
    }

    #[test]
    fn engine_step_without_driver_is_an_error() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let scn = Scenario::from_yaml(
            "name: t3\nrepo: basic\nsteps:\n  - { on: engine, wc: a, svn: [status] }\n",
        )?;
        let backend = FakeBackend {
            calls: RefCell::new(Vec::new()),
        };
        let err = Runner::new(&backend, tmp.path().to_path_buf())
            .run(&scn)
            .err();
        assert!(err.is_some_and(|e| e.to_string().contains("no EngineDriver")));
        Ok(())
    }

    #[test]
    fn detects_tree_mismatch() -> Result<()> {
        struct Writer;
        impl Backend for Writer {
            fn exec(&self, _t: Target, _cwd: &str, argv: &[String]) -> Result<CmdOutput> {
                Ok(CmdOutput {
                    code: 0,
                    stdout: argv.join(" "),
                    stderr: String::new(),
                })
            }
        }
        let tmp = tempfile::tempdir()?;
        let scn = Scenario::from_yaml(
            "name: t4\nrepo: r\nsteps:\n  - { on: svn15, wc: a, sh: ls }\n  - { on: svn15, wc: b, sh: ls }\ncompare:\n  - { left: a, right: b, checks: [tree] }\n",
        )?;
        // Pre-create a stray file in b after the runner cleans: emulate via engine-free hook.
        let runner = Runner::new(&Writer, tmp.path().to_path_buf());
        let first = runner.run(&scn)?;
        assert!(first.passed());
        fs::write(tmp.path().join("t4/b/stray"), b"x")?;
        // Second compare-only evaluation using the same host dir.
        let mut report = Report::default();
        runner.compare(&scn, &scn.compare[0], &tmp.path().join("t4"), &mut report)?;
        assert_eq!(report.mismatches.len(), 1);
        Ok(())
    }

    #[test]
    fn expect_mismatch_inverts_and_flags_lax_comparators() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let backend = FakeBackend {
            calls: RefCell::new(Vec::new()),
        };
        let runner = Runner::new(&backend, tmp.path().to_path_buf());
        let scn = Scenario::from_yaml(
            "name: t5\nrepo: r\nexpect: mismatch\nsteps:\n  - { on: svn15, wc: a, sh: ls }\n  - { on: svn15, wc: b, sh: ls }\ncompare:\n  - { left: a, right: b, checks: [tree] }\n",
        )?;
        // Identical trees: the negative control must complain that nothing was detected.
        let report = runner.run(&scn)?;
        assert_eq!(report.mismatches.len(), 1, "{:?}", report.mismatches);
        assert!(report.mismatches[0].contains("expected a difference"));
        // A planted difference is detected, so the control passes.
        fs::write(tmp.path().join("t5/b/stray"), b"x")?;
        let mut report = Report::default();
        runner.compare(&scn, &scn.compare[0], &tmp.path().join("t5"), &mut report)?;
        assert!(report.passed(), "{:?}", report.mismatches);
        Ok(())
    }
}

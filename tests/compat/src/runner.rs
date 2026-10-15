//! Scenario runner: executes steps through a [`Backend`] (docker compose exec by default) and
//! evaluates the comparisons.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use crate::compare::{diff_lines_unordered, diff_text, diff_trees, normalize_xml, snapshot_tree};
use crate::scenario::{Actor, Check, Compare, Expect, Ignored, Scenario, Step, SvnVersion};
use crate::wcdb_dump::{DumpOptions, dump_wc_db_with, work_queue_rows};

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
///
/// The defaults describe the compose environment: repositories in `/var/svn/repos` of the
/// server containers, working copies below `/work` in the client containers. [`LocalBackend`]
/// overrides them for servers and clients running on the host.
pub trait Backend {
    /// Run `argv` on `target` with working directory `cwd` (a path as the client sees it).
    fn exec(&self, target: Target, cwd: &str, argv: &[String]) -> Result<CmdOutput>;

    /// URL of repository `repo` on `server`, as clients (and the engine) use it.
    fn repo_url(&self, server: SvnVersion, repo: &str) -> String {
        format!("https://{}.test.local:8443/svn/{repo}", server.tag())
    }

    /// Path of `<work_root>/<scenario>/<wc>` as a client sees it.
    fn client_path(&self, _work_root: &Path, scenario: &str, wc: &str) -> String {
        format!("{CONTAINER_WORK}/{scenario}/{wc}")
    }

    /// Whether the engine (running on the host) shares working copies and server URLs with the
    /// clients of this backend, so `on: engine` steps may run.
    fn hosts_engine(&self) -> bool {
        false
    }

    /// Copy fixture repository `from` to a new repository `to` on `server`.
    fn copy_repo(&self, server: SvnVersion, from: &str, to: &str) -> Result<()> {
        server_sh(
            self,
            server,
            &format!(
                "svnadmin hotcopy /var/svn/repos/'{from}' /var/svn/repos/'{to}' && chown -R www-data:www-data /var/svn/repos/'{to}'"
            ),
        )
    }

    /// Delete repository `name` on `server` (best effort).
    fn remove_repo(&self, server: SvnVersion, name: &str) -> Result<()> {
        server_sh(self, server, &format!("rm -rf /var/svn/repos/'{name}'"))
    }
}

fn server_sh<B: Backend + ?Sized>(backend: &B, server: SvnVersion, script: &str) -> Result<()> {
    let argv = vec!["sh".to_string(), "-c".into(), script.to_string()];
    let out = backend.exec(Target::Server(server), "/", &argv)?;
    if out.code != 0 {
        bail!("server command failed ({}): {}", out.code, out.stderr);
    }
    Ok(())
}

/// Drives the OpenSVN engine for `on: engine` steps.
pub trait EngineDriver {
    /// Execute an `svn`-style step against the working copy directory on the host. An `Err`
    /// is a failed step (or, with `fails_with`, the expected failure: its text is matched).
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

        if self.engine.is_some()
            && !self.backend.hosts_engine()
            && scn.steps.iter().any(|s| s.on == Actor::Engine)
        {
            bail!(
                "scenario {} has engine steps, which need the local backend (servers and clients on the host, see tests/compat/README.md)",
                scn.name
            );
        }
        let repo_name = if scn.fresh_repo {
            let name = format!("{}-{}", scn.name, std::process::id());
            self.backend
                .copy_repo(scn.server, &scn.repo, &name)
                .with_context(|| {
                    format!(
                        "copy fixture repo '{}' (did `cargo xtask compat fixtures` run?)",
                        scn.repo
                    )
                })?;
            name
        } else {
            scn.repo.clone()
        };
        let result = self.run_inner(scn, &repo_name, &host_dir);
        if scn.fresh_repo {
            // Best effort: a leftover copy only wastes volume space.
            let _ = self.backend.remove_repo(scn.server, &repo_name);
        }
        result
    }

    fn run_inner(&self, scn: &Scenario, repo_name: &str, host_dir: &Path) -> Result<Report> {
        let url = self.backend.repo_url(scn.server, repo_name);
        let mut report = Report {
            scenario: scn.name.clone(),
            ..Report::default()
        };
        for (i, step) in scn.steps.iter().enumerate() {
            let wc_host = host_dir.join(&step.wc);
            fs::create_dir_all(&wc_host)?;
            let wc_container = self
                .backend
                .client_path(&self.work_root, &scn.name, &step.wc);
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
                    if args.is_empty() {
                        bail!("step {i}: engine steps take `svn:` arguments");
                    }
                    let res = driver.run(step, &wc_host, &args);
                    match (&step.fails_with, res) {
                        (None, res) => res.with_context(|| {
                            format!("step {i} (engine {})", args.join(" "))
                        })?,
                        (Some(needle), Err(e)) if format!("{e:#}").contains(needle.as_str()) => {}
                        (Some(needle), Err(e)) => bail!(
                            "step {i} (engine {}) should fail with '{needle}' but failed with: {e:#}",
                            args.join(" ")
                        ),
                        (Some(needle), Ok(())) => bail!(
                            "step {i} (engine {}) should fail with '{needle}' but succeeded",
                            args.join(" ")
                        ),
                    }
                }
                Actor::Svn18 | Actor::Svn19 | Actor::Svn110 | Actor::Svn14 | Actor::Svn15 => {
                    let version = step
                        .on
                        .svn_version()
                        .ok_or_else(|| anyhow!("step {i}: actor has no svn version"))?;
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
                    if let Some(needle) = &step.fails_with {
                        if out.code == 0 || !out.stderr.contains(needle.as_str()) {
                            bail!(
                                "step {i} ({:?} {}) should fail with '{needle}' but exited {}\nstdout: {}\nstderr: {}",
                                step.on,
                                argv.join(" "),
                                out.code,
                                out.stdout,
                                out.stderr
                            );
                        }
                    } else if out.code != 0 {
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
                Check::Wcdb => {
                    let opts = DumpOptions {
                        mask_dav_cache: cmp.ignore.contains(&Ignored::DavCache),
                    };
                    diff_lines_unordered(
                        &dump_wc_db_with(&host_dir.join(&cmp.left), opts)?,
                        &dump_wc_db_with(&host_dir.join(&cmp.right), opts)?,
                    )
                }
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
        let cwd = self.backend.client_path(&self.work_root, &scn.name, wc);
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

    /// Exits with a fixed code and stderr for every command.
    struct Fixed {
        code: i32,
        stderr: &'static str,
    }

    impl Backend for Fixed {
        fn exec(&self, _t: Target, _cwd: &str, _argv: &[String]) -> Result<CmdOutput> {
            Ok(CmdOutput {
                code: self.code,
                stdout: String::new(),
                stderr: self.stderr.to_string(),
            })
        }
    }

    #[test]
    fn fails_with_needs_nonzero_exit_and_matching_stderr() -> Result<()> {
        let scn = Scenario::from_yaml(
            "name: t6\nrepo: r\nsteps:\n  - { on: svn18, wc: a, svn: [status], fails_with: E155021 }\n",
        )?;
        let refused = Fixed {
            code: 1,
            stderr: "svn: E155021: This client is too old to work with the working copy (format 32).",
        };
        let tmp = tempfile::tempdir()?;
        let report = Runner::new(&refused, tmp.path().to_path_buf()).run(&scn)?;
        assert_eq!(report.steps_run, 1);
        // Success is a failure of the negative step.
        let ok = Fixed {
            code: 0,
            stderr: "",
        };
        let tmp = tempfile::tempdir()?;
        let err = Runner::new(&ok, tmp.path().to_path_buf()).run(&scn).err();
        assert!(err.is_some_and(|e| e.to_string().contains("should fail with")));
        // A different error is not accepted either.
        let other = Fixed {
            code: 1,
            stderr: "svn: E170013: Unable to connect",
        };
        let tmp = tempfile::tempdir()?;
        let err = Runner::new(&other, tmp.path().to_path_buf())
            .run(&scn)
            .err();
        assert!(err.is_some_and(|e| e.to_string().contains("E170013")));
        Ok(())
    }
}

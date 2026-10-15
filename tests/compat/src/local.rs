//! [`LocalBackend`]: scenarios against Apache + `mod_dav_svn` servers and svn clients running on
//! the host instead of in the compose containers (`scripts/test-env/visualsvn-like.sh up`).
//!
//! The engine runs on the host too, so only this backend can run `on: engine` steps: working
//! copies, URLs and the test CA are the same for the engine and both svn clients, and a working
//! copy written by one is opened in place by the others.
//!
//! Environment (the exports of `visualsvn-like.sh env` cover the server part):
//!
//! | Variable | Meaning |
//! |---|---|
//! | `OPENSVN_COMPAT_LOCAL_URL14` | parent-path URL served by mod_dav_svn 1.14.x, e.g. `https://svn.opensvn.test:18443/svn-basic` |
//! | `OPENSVN_COMPAT_LOCAL_URL15` | parent-path URL served by mod_dav_svn 1.15.0, e.g. `https://svn.opensvn.test:18444/svn-basic` |
//! | `OPENSVN_COMPAT_LOCAL_REPOS` | the `SVNParentPath` directory of both |
//! | `OPENSVN_COMPAT_LOCAL_SVNADMIN` | `svnadmin` to copy repositories with (default `svnadmin`) |
//! | `OPENSVN_COMPAT_LOCAL_CA` | PEM of the test CA |
//! | `OPENSVN_COMPAT_SVN14_BIN` | svn 1.14.x client (default `svn`) |
//! | `OPENSVN_COMPAT_SVN15_BIN` | svn 1.15.0 client |
//!
//! The server must accept the Basic account [`TEST_USER`](crate::runner::TEST_USER).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::runner::{Backend, CmdOutput, Target};
use crate::scenario::SvnVersion;

/// Host servers and clients (see the module docs).
#[derive(Debug, Clone)]
pub struct LocalBackend {
    svn14: PathBuf,
    svn15: PathBuf,
    url14: String,
    url15: String,
    repos: PathBuf,
    svnadmin: PathBuf,
    ca: PathBuf,
    config_dir: PathBuf,
}

fn env_var(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is not set (see tests/compat/README.md)"))
}

impl LocalBackend {
    /// Reads the configuration from the environment. `config_dir` becomes the svn clients'
    /// `--config-dir` (created; keeps the user's own svn configuration and auth cache out).
    pub fn from_env(config_dir: &Path) -> Result<Self> {
        let backend = Self {
            svn14: std::env::var_os("OPENSVN_COMPAT_SVN14_BIN")
                .map_or_else(|| PathBuf::from("svn"), PathBuf::from),
            svn15: PathBuf::from(env_var("OPENSVN_COMPAT_SVN15_BIN")?),
            url14: env_var("OPENSVN_COMPAT_LOCAL_URL14")?
                .trim_end_matches('/')
                .to_owned(),
            url15: env_var("OPENSVN_COMPAT_LOCAL_URL15")?
                .trim_end_matches('/')
                .to_owned(),
            repos: PathBuf::from(env_var("OPENSVN_COMPAT_LOCAL_REPOS")?),
            svnadmin: std::env::var_os("OPENSVN_COMPAT_LOCAL_SVNADMIN")
                .map_or_else(|| PathBuf::from("svnadmin"), PathBuf::from),
            ca: PathBuf::from(env_var("OPENSVN_COMPAT_LOCAL_CA")?),
            config_dir: config_dir.to_path_buf(),
        };
        std::fs::create_dir_all(&backend.config_dir)
            .with_context(|| format!("create {}", backend.config_dir.display()))?;
        Ok(backend)
    }

    /// Whether the environment names a local setup (both server URLs set).
    pub fn configured() -> bool {
        std::env::var_os("OPENSVN_COMPAT_LOCAL_URL14").is_some()
            && std::env::var_os("OPENSVN_COMPAT_LOCAL_URL15").is_some()
    }

    /// The test CA (PEM), for the engine's trust store.
    pub fn ca(&self) -> &Path {
        &self.ca
    }

    /// The svn client of `version`.
    pub fn svn_bin(&self, version: SvnVersion) -> Result<&Path> {
        match version {
            SvnVersion::V14 => Ok(&self.svn14),
            SvnVersion::V15 => Ok(&self.svn15),
            other => bail!("the local backend has no {} client", other.tag()),
        }
    }

    /// Options every svn invocation gets: private config dir, the test CA, no auth cache.
    pub fn svn_options(&self) -> Vec<String> {
        vec![
            "--config-dir".into(),
            self.config_dir.display().to_string(),
            "--config-option".into(),
            format!(
                "servers:global:ssl-authority-files={}",
                self.ca.display()
            ),
            "--no-auth-cache".into(),
        ]
    }

    fn run(cmd: &mut Command) -> Result<CmdOutput> {
        let out = cmd
            .output()
            .with_context(|| format!("failed to spawn {cmd:?}"))?;
        Ok(CmdOutput {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

impl Backend for LocalBackend {
    fn exec(&self, target: Target, cwd: &str, argv: &[String]) -> Result<CmdOutput> {
        let Target::Client(version) = target else {
            bail!("the local backend runs no commands on the server ({argv:?})");
        };
        let Some((program, args)) = argv.split_first() else {
            bail!("empty command");
        };
        let mut cmd = if program == "svn" {
            let mut c = Command::new(self.svn_bin(version)?);
            c.args(args).args(self.svn_options());
            c
        } else {
            let mut c = Command::new(program);
            c.args(args);
            c
        };
        // English messages, UTF-8 paths: stderr is matched by `fails_with`.
        cmd.current_dir(cwd)
            .env("LC_ALL", "C.UTF-8")
            .env("LANG", "C.UTF-8");
        Self::run(&mut cmd)
    }

    fn repo_url(&self, server: SvnVersion, repo: &str) -> String {
        match server {
            SvnVersion::V14 => format!("{}/{repo}", self.url14),
            _ => format!("{}/{repo}", self.url15),
        }
    }

    fn client_path(&self, work_root: &Path, scenario: &str, wc: &str) -> String {
        // svn prints canonical paths; `normalize_xml` replaces this one with `{WC}`.
        let root = work_root
            .canonicalize()
            .unwrap_or_else(|_| work_root.to_path_buf());
        root.join(scenario).join(wc).display().to_string()
    }

    fn hosts_engine(&self) -> bool {
        true
    }

    fn copy_repo(&self, _server: SvnVersion, from: &str, to: &str) -> Result<()> {
        let out = Self::run(
            Command::new(&self.svnadmin)
                .arg("hotcopy")
                .arg(self.repos.join(from))
                .arg(self.repos.join(to)),
        )?;
        if out.code != 0 {
            bail!("svnadmin hotcopy {from} {to}: {}", out.stderr);
        }
        Ok(())
    }

    fn remove_repo(&self, _server: SvnVersion, name: &str) -> Result<()> {
        let dir = self.repos.join(name);
        std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))
    }
}

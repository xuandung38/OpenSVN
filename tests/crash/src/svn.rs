//! The native `svn` / `svnadmin` (via `opensvn-svn-oracle`: isolated config directory, C locale)
//! used to set up fixtures and as the oracle that judges a working copy after a crash.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use opensvn_svn_oracle::{MinVersion, Oracle};

use crate::{Result, err};

/// The installed Subversion command line client.
#[derive(Debug, Clone)]
pub struct Svn {
    oracle: Oracle,
}

fn run(mut cmd: Command, what: &str) -> Result<Vec<u8>> {
    let out = cmd
        .output()
        .map_err(|e| err(format!("{what}: cannot run: {e}")))?;
    if !out.status.success() {
        return Err(err(format!(
            "{what}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

impl Svn {
    /// The installed svn (1.14 or newer), `None` when there is none (tests skip).
    pub fn find() -> Result<Option<Self>> {
        Oracle::resolve(MinVersion::V1_14)
            .map(|o| o.map(|oracle| Self { oracle }))
            .map_err(|e| err(e.to_string()))
    }

    /// `(major, minor, patch)`.
    pub fn version(&self) -> (u32, u32, u32) {
        self.oracle.version()
    }

    /// A ready `svn` command (isolated config, non-interactive).
    pub fn cmd(&self) -> Command {
        self.oracle.svn()
    }

    /// Runs `svn args...` in `cwd` and returns stdout.
    pub fn run_in(&self, cwd: &Path, args: &[&str]) -> Result<String> {
        let mut c = self.cmd();
        c.current_dir(cwd).args(args);
        run(c, &format!("svn {}", args.join(" "))).map(|o| String::from_utf8_lossy(&o).into_owned())
    }

    /// Runs `svn args...` and returns stdout.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        let mut c = self.cmd();
        c.args(args);
        run(c, &format!("svn {}", args.join(" "))).map(|o| String::from_utf8_lossy(&o).into_owned())
    }

    /// `svnadmin create <path>`.
    pub fn create_repo(&self, path: &Path) -> Result<()> {
        let mut c = self.oracle.svnadmin();
        c.arg("create").arg(path);
        run(c, "svnadmin create").map(|_| ())
    }

    /// `svnadmin load <repo> < dump`.
    pub fn load(&self, repo: &Path, dump: &Path) -> Result<()> {
        let mut c = self.oracle.svnadmin();
        c.args(["load", "-q"]).arg(repo);
        c.stdin(
            std::fs::File::open(dump).map_err(|e| err(format!("open {}: {e}", dump.display())))?,
        );
        run(c, "svnadmin load").map(|_| ())
    }

    /// The youngest revision of the repository at `url`.
    pub fn youngest(&self, url: &str) -> Result<u64> {
        let out = self.run(&["info", "--show-item", "revision", "-r", "HEAD", url])?;
        out.trim()
            .parse()
            .map_err(|e| err(format!("svn info revision {out:?}: {e}")))
    }

    /// File contents at `url` (HEAD).
    pub fn cat(&self, url: &str) -> Result<Vec<u8>> {
        let mut c = self.cmd();
        c.args(["cat", url]);
        run(c, "svn cat")
    }

    /// `svn status` of the working copy as `path -> columns` (first, second and seventh column:
    /// item, properties, tree conflict). Unmodified items are not listed. Fails when svn cannot
    /// read the working copy (corrupt database, unfinished work queue).
    pub fn status(&self, wc: &Path) -> Result<BTreeMap<String, String>> {
        let out = self.run_in(wc, &["status", "--ignore-externals"])?;
        Ok(parse_status(&out))
    }

    /// `svn diff` of the working copy (local changes against BASE).
    pub fn diff(&self, wc: &Path) -> Result<String> {
        self.run_in(wc, &["diff", "--internal-diff"])
    }
}

/// Parses plain `svn status` output into `path -> "XYC"` (item, props, tree conflict columns).
pub fn parse_status(out: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in out.lines() {
        if line.len() < 9 || line.starts_with("Summary of conflicts") || line.starts_with("  ") {
            continue;
        }
        let cols: Vec<char> = line.chars().take(7).collect();
        if cols.len() < 7 {
            continue;
        }
        let path: String = line.chars().skip(8).collect();
        // ` > ` lines carry tree conflict details; they start with spaces and were skipped.
        let code: String = [cols[0], cols[1], cols[6]].iter().collect();
        if code.trim().is_empty() {
            continue; // only lock/switch columns set: not a content difference
        }
        map.insert(path.replace('\\', "/"), code);
    }
    map
}

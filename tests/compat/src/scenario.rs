//! Scenario DSL: YAML files describing steps executed by svn14 / svn15 / engine / a human,
//! followed by comparisons between working copies.
//!
//! ```yaml
//! name: example
//! server: svn15          # which server container hosts the repo (default svn15)
//! repo: basic            # fixture repo name under /var/svn/repos
//! fresh_repo: true       # hotcopy the fixture so commits never leak into other scenarios
//! steps:
//!   - { on: svn15, wc: a, svn: [checkout, "{url}/trunk", "."] }
//!   - { on: svn15, wc: a, sh: "echo hi >> README.txt" }
//! compare:
//!   - { left: a, right: b, checks: [tree, status, info, wcdb] }
//! ```
//! Placeholders usable in `svn` args and `sh` commands: `{url}` (repo URL), `{repo}` (repo
//! name), `{wc}` (container path of the step's working copy).
//!
//! A step with `fails_with: E155021` is a negative step: it passes only when the command exits
//! non-zero and its stderr contains the given text.

use std::path::Path;

use serde::Deserialize;

/// Which implementation performs a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Actor {
    /// svn CLI 1.8.19 in a container (legacy profile).
    #[serde(rename = "svn18")]
    Svn18,
    /// svn CLI 1.9.12 in a container (legacy profile).
    #[serde(rename = "svn19")]
    Svn19,
    /// svn CLI 1.10.8 in a container (legacy profile).
    #[serde(rename = "svn110")]
    Svn110,
    /// svn CLI 1.14.5 in a container.
    #[serde(rename = "svn14")]
    Svn14,
    /// svn CLI 1.15.0 in a container.
    #[serde(rename = "svn15")]
    Svn15,
    /// The OpenSVN engine under test (driven through an `EngineDriver`).
    #[serde(rename = "engine")]
    Engine,
    /// A human performs the step with TortoiseSVN; the runner only records the note.
    #[serde(rename = "tortoise-manual")]
    TortoiseManual,
}

/// Server/client Subversion version selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum SvnVersion {
    /// Subversion 1.8.19 (legacy profile).
    #[serde(rename = "svn18")]
    V18,
    /// Subversion 1.9.12 (legacy profile).
    #[serde(rename = "svn19")]
    V19,
    /// Subversion 1.10.8 (legacy profile).
    #[serde(rename = "svn110")]
    V110,
    /// Subversion 1.14.5.
    #[serde(rename = "svn14")]
    V14,
    /// Subversion 1.15.0.
    #[serde(rename = "svn15")]
    V15,
}

impl SvnVersion {
    /// Short tag used in compose service names, e.g. `svn15`.
    pub fn tag(self) -> &'static str {
        match self {
            Self::V18 => "svn18",
            Self::V19 => "svn19",
            Self::V110 => "svn110",
            Self::V14 => "svn14",
            Self::V15 => "svn15",
        }
    }
}

impl Actor {
    /// The svn version this actor runs, for actors that are svn CLIs.
    pub fn svn_version(self) -> Option<SvnVersion> {
        match self {
            Self::Svn18 => Some(SvnVersion::V18),
            Self::Svn19 => Some(SvnVersion::V19),
            Self::Svn110 => Some(SvnVersion::V110),
            Self::Svn14 => Some(SvnVersion::V14),
            Self::Svn15 => Some(SvnVersion::V15),
            Self::Engine | Self::TortoiseManual => None,
        }
    }
}

/// One scenario step. Exactly one of `svn`, `sh`, `note` must be set (validated on load).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Who performs the step.
    pub on: Actor,
    /// Working copy name (a directory under the scenario work dir).
    pub wc: String,
    /// svn subcommand and arguments.
    #[serde(default)]
    pub svn: Option<Vec<String>>,
    /// Shell command run inside the working copy directory.
    #[serde(default)]
    pub sh: Option<String>,
    /// Instructions for `tortoise-manual` steps.
    #[serde(default)]
    pub note: Option<String>,
    /// Negative step: the command must exit non-zero and its stderr must contain this text
    /// (e.g. `E155021` when an old client refuses a newer working copy format).
    #[serde(default)]
    pub fails_with: Option<String>,
}

/// What a comparison inspects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Check {
    /// File tree: paths, contents, exec bit (excludes `.svn`).
    Tree,
    /// Normalised `svn status --xml`.
    Status,
    /// Normalised `svn info --xml`.
    Info,
    /// Normalised `wc.db` dump.
    Wcdb,
}

fn all_checks() -> Vec<Check> {
    vec![Check::Tree, Check::Status, Check::Info, Check::Wcdb]
}

/// A comparison between two working copies.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compare {
    /// Left working copy name.
    pub left: String,
    /// Right working copy name.
    pub right: String,
    /// Checks to run (default: all).
    #[serde(default = "all_checks")]
    pub checks: Vec<Check>,
    /// svn version used to read `status`/`info` from both working copies (default svn15, so
    /// format-32 working copies are never read by 1.14).
    #[serde(default = "default_reader")]
    pub reader: SvnVersion,
}

fn default_reader() -> SvnVersion {
    SvnVersion::V15
}

/// Whether the comparisons are expected to agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Expect {
    /// Every check must find no difference (default).
    #[default]
    Match,
    /// Negative control: every listed check of every comparison must find a difference.
    /// Proves the comparators really detect changes (guards against over-normalising).
    Mismatch,
}

fn default_server() -> SvnVersion {
    SvnVersion::V15
}

/// A parsed scenario file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Unique scenario name (used for work dirs and repo copies).
    pub name: String,
    /// Free-form description.
    #[serde(default)]
    pub description: Option<String>,
    /// Server version hosting the repository.
    #[serde(default = "default_server")]
    pub server: SvnVersion,
    /// Fixture repository name on the server.
    pub repo: String,
    /// Hotcopy the fixture repo first so the scenario may write to it.
    #[serde(default)]
    pub fresh_repo: bool,
    /// Steps, executed in order.
    pub steps: Vec<Step>,
    /// Comparisons run after all steps.
    #[serde(default)]
    pub compare: Vec<Compare>,
    /// `match` (default) or `mismatch` for negative controls.
    #[serde(default)]
    pub expect: Expect,
}

/// Scenario loading problems.
#[derive(Debug, thiserror::Error)]
pub enum ScenarioError {
    /// File could not be read.
    #[error("cannot read scenario {path}: {source}")]
    Read {
        /// File path.
        path: String,
        /// Underlying error.
        source: std::io::Error,
    },
    /// YAML did not match the schema.
    #[error("invalid scenario {path}: {source}")]
    Parse {
        /// File path.
        path: String,
        /// Underlying error.
        source: Box<serde_saphyr::Error>,
    },
    /// Schema-valid but semantically wrong.
    #[error("invalid scenario {name}: {reason}")]
    Invalid {
        /// Scenario name.
        name: String,
        /// What is wrong.
        reason: String,
    },
}

/// A name that is safe as a single path component and repo name.
fn is_safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl Scenario {
    /// Parse and validate a scenario from YAML text.
    pub fn from_yaml(text: &str) -> Result<Self, ScenarioError> {
        let scenario: Self =
            serde_saphyr::from_str(text).map_err(|source| ScenarioError::Parse {
                path: "<inline>".into(),
                source: Box::new(source),
            })?;
        scenario.validate()?;
        Ok(scenario)
    }

    /// Load and validate a scenario file.
    pub fn load(path: &Path) -> Result<Self, ScenarioError> {
        let display = path.display().to_string();
        let text = std::fs::read_to_string(path).map_err(|source| ScenarioError::Read {
            path: display.clone(),
            source,
        })?;
        let scenario: Self =
            serde_saphyr::from_str(&text).map_err(|source| ScenarioError::Parse {
                path: display,
                source: Box::new(source),
            })?;
        scenario.validate()?;
        Ok(scenario)
    }

    /// Names are used in file paths and shell-adjacent contexts, so restrict them.
    fn validate(&self) -> Result<(), ScenarioError> {
        let bad = |reason: String| ScenarioError::Invalid {
            name: self.name.clone(),
            reason,
        };
        if !is_safe_name(&self.name) {
            return Err(bad("name must match [A-Za-z0-9_-]{1,64}".into()));
        }
        if !is_safe_name(&self.repo) {
            return Err(bad("repo must match [A-Za-z0-9_-]{1,64}".into()));
        }
        if self.steps.is_empty() {
            return Err(bad("at least one step required".into()));
        }
        for (i, step) in self.steps.iter().enumerate() {
            if !is_safe_name(&step.wc) {
                return Err(bad(format!(
                    "step {i}: wc must match [A-Za-z0-9_-]{{1,64}}"
                )));
            }
            let set = [step.svn.is_some(), step.sh.is_some(), step.note.is_some()]
                .iter()
                .filter(|b| **b)
                .count();
            if set != 1 {
                return Err(bad(format!(
                    "step {i}: exactly one of svn/sh/note required"
                )));
            }
            if step.on == Actor::TortoiseManual && step.note.is_none() {
                return Err(bad(format!("step {i}: tortoise-manual needs a note")));
            }
            if step.on != Actor::TortoiseManual && step.note.is_some() {
                return Err(bad(format!(
                    "step {i}: note is only valid for tortoise-manual"
                )));
            }
            if let Some(needle) = &step.fails_with {
                if step.svn.is_none() && step.sh.is_none() {
                    return Err(bad(format!("step {i}: fails_with needs svn or sh")));
                }
                if step.on == Actor::Engine || needle.trim().is_empty() {
                    return Err(bad(format!(
                        "step {i}: fails_with needs non-empty text and an svn actor"
                    )));
                }
            }
            if step.svn.as_ref().is_some_and(Vec::is_empty) {
                return Err(bad(format!("step {i}: svn args are empty")));
            }
        }
        if self.expect == Expect::Mismatch && self.compare.is_empty() {
            return Err(bad("expect: mismatch needs at least one compare".into()));
        }
        for c in &self.compare {
            for side in [&c.left, &c.right] {
                if !self.steps.iter().any(|s| &s.wc == side) {
                    return Err(bad(format!("compare references unknown wc '{side}'")));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"
name: demo
repo: basic
steps:
  - { on: svn15, wc: a, svn: [checkout, "{url}/trunk", "."] }
compare:
  - { left: a, right: a, checks: [tree] }
"#;

    #[test]
    fn parses_valid_scenario_with_defaults() -> anyhow::Result<()> {
        let s = Scenario::from_yaml(OK)?;
        assert_eq!(s.server, SvnVersion::V15);
        assert!(!s.fresh_repo);
        assert_eq!(s.compare[0].checks, vec![Check::Tree]);
        Ok(())
    }

    #[test]
    fn expect_and_reader_parse_with_defaults() -> anyhow::Result<()> {
        let s = Scenario::from_yaml(&format!("{OK}expect: mismatch\n"))?;
        assert_eq!(s.expect, Expect::Mismatch);
        assert_eq!(s.compare[0].reader, SvnVersion::V15);
        assert_eq!(Scenario::from_yaml(OK)?.expect, Expect::Match);
        let no_compare =
            "name: n\nrepo: r\nexpect: mismatch\nsteps:\n  - { on: svn15, wc: a, sh: ls }\n";
        assert!(matches!(
            Scenario::from_yaml(no_compare),
            Err(ScenarioError::Invalid { .. })
        ));
        Ok(())
    }

    #[test]
    fn rejects_path_traversal_in_names() {
        let bad = OK.replace("name: demo", "name: ../x");
        assert!(matches!(
            Scenario::from_yaml(&bad),
            Err(ScenarioError::Invalid { .. })
        ));
    }

    #[test]
    fn rejects_step_with_two_actions() {
        let bad = OK.replace("svn: [", "sh: ls, svn: [");
        assert!(matches!(
            Scenario::from_yaml(&bad),
            Err(ScenarioError::Invalid { .. })
        ));
    }

    #[test]
    fn rejects_unknown_fields_and_unknown_wc() {
        assert!(matches!(
            Scenario::from_yaml(&OK.replace("repo: basic", "repo: basic\nbogus: 1")),
            Err(ScenarioError::Parse { .. })
        ));
        assert!(matches!(
            Scenario::from_yaml(&OK.replace("right: a", "right: zzz")),
            Err(ScenarioError::Invalid { .. })
        ));
    }

    #[test]
    fn tortoise_manual_requires_note() {
        let y = "name: t\nrepo: r\nsteps:\n  - { on: tortoise-manual, wc: a, sh: ls }\n";
        assert!(matches!(
            Scenario::from_yaml(y),
            Err(ScenarioError::Invalid { .. })
        ));
        let y = "name: t\nrepo: r\nsteps:\n  - { on: tortoise-manual, wc: a, note: click }\n";
        assert!(Scenario::from_yaml(y).is_ok());
    }

    #[test]
    fn fails_with_is_validated() {
        let ok = "name: n\nrepo: r\nsteps:\n  - { on: svn18, wc: a, svn: [status], fails_with: E155021 }\n";
        assert!(Scenario::from_yaml(ok).is_ok());
        let empty =
            "name: n\nrepo: r\nsteps:\n  - { on: svn18, wc: a, svn: [status], fails_with: '' }\n";
        assert!(Scenario::from_yaml(empty).is_err());
        let engine =
            "name: n\nrepo: r\nsteps:\n  - { on: engine, wc: a, svn: [status], fails_with: E1 }\n";
        assert!(Scenario::from_yaml(engine).is_err());
    }
}

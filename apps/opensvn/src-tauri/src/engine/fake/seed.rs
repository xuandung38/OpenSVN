//! Demo data: a small repository and a working copy with a variety of local states, so the UI
//! shows realistic content without any server.

use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use opensvn_ipc::{Depth, Rev, WcFormat};
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_types::{PropChange, RelPath, SvnError};

use super::FakeEngine;
use crate::engine::context::OpContext;
use crate::engine::facade::{CheckoutPlan, EngineFacade, LockScope, WcRef};

/// URL of the demo repository.
pub const DEMO_REPO_URL: &str = "https://svn.test.local/repos/demo";
const DEMO_UUID: &str = "6c0f5d2e-0000-4000-8000-0000000000d1";

/// Root of the demo working copy (an absolute path that never exists on disk).
pub fn demo_wc_root() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\opensvn-fake\demo")
    } else {
        PathBuf::from("/opensvn-fake/demo")
    }
}

fn put(path: &str, content: &str) -> Result<FakeChange, SvnError> {
    Ok(FakeChange::PutFile(
        RelPath::new(path)?,
        Bytes::from(content.to_owned()),
    ))
}

fn dir(path: &str) -> Result<FakeChange, SvnError> {
    Ok(FakeChange::PutDir(RelPath::new(path)?))
}

const MAIN_V1: &str = "fn main() {\n    println!(\"hello\");\n}\n";
const MAIN_V2: &str =
    "fn main() {\n    println!(\"hello, world\");\n    run();\n}\n\nfn run() {}\n";

/// Builds the demo repository (10 revisions).
pub fn demo_repo() -> Result<FakeRaSession, SvnError> {
    let repo = FakeRaSession::new(DEMO_REPO_URL, DEMO_UUID)?.with_user("alice");
    repo.seed(
        "alice",
        "Initial layout",
        &[
            dir("trunk")?,
            dir("branches")?,
            dir("tags")?,
            put("trunk/README.md", "# Demo\n\nA small project.\n")?,
            put("trunk/src/main.rs", MAIN_V1)?,
            put(
                "trunk/src/lib.rs",
                "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
            )?,
        ],
    )?;
    repo.seed(
        "bob",
        "Add documentation and logo",
        &[
            put("trunk/docs/guide.md", "# Guide\n\nRead the README first.\n")?,
            FakeChange::PutFile(
                RelPath::new("trunk/assets/logo.png")?,
                Bytes::from_static(&[0x89, b'P', b'N', b'G', 0, 1, 2, 3]),
            ),
        ],
    )?;
    repo.seed(
        "alice",
        "Print a friendlier greeting",
        &[put("trunk/src/main.rs", MAIN_V2)?],
    )?;
    repo.seed(
        "carol",
        "Create feature-x branch",
        // A copy of trunk (so switching between the two passes the ancestry check), trimmed to
        // the README and `main.rs`.
        &[
            FakeChange::Copy {
                from: RelPath::new("trunk")?,
                rev: svn_types::Revnum(3),
                to: RelPath::new("branches/feature-x")?,
            },
            FakeChange::Delete(RelPath::new("branches/feature-x/src/lib.rs")?),
            FakeChange::Delete(RelPath::new("branches/feature-x/docs")?),
            FakeChange::Delete(RelPath::new("branches/feature-x/assets")?),
        ],
    )?;
    repo.seed(
        "carol",
        "Experiment on feature-x",
        &[put(
            "branches/feature-x/src/main.rs",
            "fn main() {\n    experiment();\n}\n\nfn experiment() {}\n",
        )?],
    )?;
    repo.seed(
        "alice",
        "Tag 1.0",
        &[
            dir("tags/v1.0")?,
            put("tags/v1.0/README.md", "# Demo\n\nA small project.\n")?,
        ],
    )?;
    repo.seed(
        "bob",
        "Document the build",
        &[put(
            "trunk/README.md",
            "# Demo\n\nA small project.\n\n## Build\n\nRun `cargo build`.\n",
        )?],
    )?;
    repo.seed("alice", "Add a multiply helper", &[put(
        "trunk/src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn mul(a: i32, b: i32) -> i32 {\n    a * b\n}\n",
    )?])?;
    repo.seed(
        "bob",
        "Expand the guide",
        &[
            put(
                "trunk/docs/guide.md",
                "# Guide\n\nRead the README first.\n\nThen run the tests.\n",
            )?,
            FakeChange::SetProp(
                RelPath::new("trunk/docs/guide.md")?,
                PropChange {
                    name: "svn:eol-style".into(),
                    value: Some(Bytes::from_static(b"native")),
                },
            ),
        ],
    )?;
    repo.seed(
        "carol",
        "Fix greeting typo",
        &[put(
            "trunk/src/main.rs",
            &MAIN_V2.replace("hello, world", "Hello, world"),
        )?],
    )?;
    Ok(repo)
}

impl FakeEngine {
    /// Engine with the demo repository and a registered-ready demo working copy at
    /// [`demo_wc_root`], checked out at r8 (two revisions behind) with local changes.
    pub async fn demo(step_delay: Duration) -> Result<Self, SvnError> {
        let engine = Self::empty().with_repo(demo_repo()?);
        let ctx = OpContext::detached();
        let root = demo_wc_root();
        engine
            .checkout(
                &CheckoutPlan {
                    url: format!("{DEMO_REPO_URL}/trunk"),
                    path: root.clone(),
                    rev: Some(Rev(8)),
                    depth: Depth::Infinity,
                    sparse: None,
                    format: WcFormat::V31,
                    store_pristine: true,
                    ignore_externals: false,
                },
                &ctx,
            )
            .await?;
        engine.edit_file(
            &root,
            "README.md",
            b"# Demo\n\nA small project.\n\n## Build\n\nRun `cargo build --release`.\n",
        )?;
        engine.edit_file(&root, "notes.txt", b"todo: write tests\n")?;
        engine.edit_file(&root, "src/util.rs", b"pub fn noop() {}\n")?;
        engine.remove_from_disk(&root, "docs/guide.md")?;
        let wc = WcRef {
            id: opensvn_ipc::WcId(String::new()),
            root: root.clone(),
        };
        engine
            .add_paths(&wc, &["src/util.rs".to_owned()], &ctx)
            .await?;
        engine
            .changelist_set(&wc, &["README.md".to_owned()], Some("docs"), &ctx)
            .await?;
        engine
            .lock(
                &LockScope::Wc {
                    wc: wc.clone(),
                    paths: vec!["src/lib.rs".to_owned()],
                },
                Some("refactoring"),
                false,
                &ctx,
            )
            .await?;
        Ok(Self {
            step_delay,
            ..engine
        })
    }
}

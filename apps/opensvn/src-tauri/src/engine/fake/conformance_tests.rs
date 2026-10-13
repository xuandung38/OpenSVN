//! Runs the shared engine suite against the fake engine, and checks fake-specific behavior.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bytes::Bytes;
use opensvn_ipc::{
    CleanupRequest, CommitRequest, Depth, NodeStatus, Rev, UpdateRequest, WcFormat, WcId,
};
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_types::{ErrorCode, RelPath, SvnError};

use super::{FakeEngine, demo_wc_root};
use crate::engine::conformance::{self, Harness, PlantedOwner};
use crate::engine::{CheckoutPlan, EngineFacade, OpContext, ProgressReporter, StatusQuery, WcRef};

const URL: &str = "https://svn.test.local/repos/conformance";

struct FakeHarness {
    engine: FakeEngine,
    scratch: tempfile::TempDir,
}

fn work_dir() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\opensvn-fake\work")
    } else {
        PathBuf::from("/opensvn-fake/work")
    }
}

fn standard_repo() -> FakeRaSession {
    let repo = FakeRaSession::new(URL, "conformance-uuid").unwrap();
    let put =
        |p: &str, c: &str| FakeChange::PutFile(RelPath::new(p).unwrap(), Bytes::from(c.to_owned()));
    repo.seed(
        "alice",
        "Initial import",
        &[
            FakeChange::PutDir(RelPath::new("trunk").unwrap()),
            FakeChange::PutDir(RelPath::new("branches").unwrap()),
            FakeChange::PutDir(RelPath::new("tags").unwrap()),
            put("trunk/README.md", "hello\n"),
            put("trunk/src/main.rs", "fn main() {}\n"),
        ],
    )
    .unwrap();
    repo
}

fn harness() -> FakeHarness {
    FakeHarness {
        engine: FakeEngine::empty().with_repo(standard_repo()),
        scratch: tempfile::tempdir().unwrap(),
    }
}

impl Harness for FakeHarness {
    fn engine(&self) -> &dyn EngineFacade {
        &self.engine
    }
    fn repo_url(&self) -> String {
        URL.to_owned()
    }
    fn work_dir(&self) -> PathBuf {
        work_dir()
    }
    fn write_file(&self, root: &Path, rel: &str, content: &str) -> Result<(), SvnError> {
        self.engine.edit_file(root, rel, content.as_bytes())
    }
    fn scratch_dir(&self) -> PathBuf {
        self.scratch.path().to_path_buf()
    }
    fn plant_lock(&self, root: &Path, dir: &str, owner: PlantedOwner) -> Result<(), SvnError> {
        match owner {
            PlantedOwner::DeadProcess => self.engine.simulate_dead_owner_lock(root, dir),
            PlantedOwner::Foreign => self.engine.simulate_foreign_lock(root, dir),
        }
    }
}

#[tokio::test]
async fn shared_engine_suite() {
    conformance::run_all(harness).await;
}

fn checkout_req(path: &Path, rev: Option<Rev>) -> CheckoutPlan {
    CheckoutPlan {
        url: format!("{URL}/trunk"),
        path: path.to_path_buf(),
        rev,
        depth: Depth::Infinity,
        sparse: None,
        format: WcFormat::V31,
        store_pristine: true,
        ignore_externals: false,
    }
}

fn counting_ctx() -> (OpContext, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let s = seen.clone();
    let ctx = OpContext {
        interactive: true,
        cancel: svn_types::CancelToken::new(),
        progress: ProgressReporter::new(move |u| s.lock().unwrap().push(u.phase)),
    };
    (ctx, seen)
}

#[tokio::test]
async fn network_phase_cancels_immediately_and_leaves_nothing() {
    let h = FakeHarness {
        engine: FakeEngine::empty()
            .with_repo(standard_repo())
            .with_step_delay(Duration::from_millis(500)),
        scratch: tempfile::tempdir().unwrap(),
    };
    let (ctx, _) = counting_ctx();
    let cancel = ctx.cancel.clone();
    let root = work_dir().join("net-cancel");
    let task = {
        let root = root.clone();
        async move { h.engine.checkout(&checkout_req(&root, None), &ctx).await }
    };
    let canceller = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    };
    let started = Instant::now();
    let (res, ()) = tokio::join!(task, canceller);
    assert_eq!(res.unwrap_err().code, ErrorCode::Cancelled);
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "network phase must not wait out the delay"
    );
}

#[tokio::test]
async fn work_queue_cancel_stops_between_items_and_keeps_a_mixed_revision_update() {
    let engine = FakeEngine::empty()
        .with_repo(standard_repo())
        .with_step_delay(Duration::from_millis(20));
    let root = work_dir().join("queue-cancel");
    let wc = WcRef {
        id: WcId("q".into()),
        root: root.clone(),
    };
    let ctx = OpContext::detached();
    engine
        .checkout(&checkout_req(&root, Some(Rev(1))), &ctx)
        .await
        .unwrap();
    // advance the repository so the update has several items to apply
    let repo = &engine.repos[0];
    repo.seed(
        "bob",
        "Change both files",
        &[
            FakeChange::PutFile(
                RelPath::new("trunk/README.md").unwrap(),
                Bytes::from_static(b"v2\n"),
            ),
            FakeChange::PutFile(
                RelPath::new("trunk/src/main.rs").unwrap(),
                Bytes::from_static(b"fn main() { v2(); }\n"),
            ),
        ],
    )
    .unwrap();

    let (uctx, seen) = counting_ctx();
    let cancel = uctx.cancel.clone();
    let request = UpdateRequest {
        wc_id: wc.id.clone(),
        paths: None,
        rev: None,
        depth: None,
    };
    let update = engine.update(&wc, &request, &uctx);
    let canceller = async {
        // wait until the work-queue phase started, then cancel
        for _ in 0..200 {
            if seen.lock().unwrap().iter().any(|p| p == "work-queue") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
    };
    let (res, ()) = tokio::join!(update, canceller);
    assert_eq!(res.unwrap_err().code, ErrorCode::Cancelled);
    // some items were applied: the working copy is consistent and can be updated again
    let info = engine.wc_info(&root, &ctx).await.unwrap();
    assert!(info.base_rev.0 <= 2);
    engine
        .update(
            &wc,
            &UpdateRequest {
                wc_id: wc.id.clone(),
                paths: None,
                rev: None,
                depth: None,
            },
            &ctx,
        )
        .await
        .unwrap();
    let info = engine.wc_info(&root, &ctx).await.unwrap();
    assert_eq!(info.base_rev, Rev(2));
    assert!(info.rev_range.is_none());
}

#[tokio::test]
async fn demo_engine_offers_a_working_copy_with_every_kind_of_change() {
    let engine = FakeEngine::demo(Duration::ZERO).await.unwrap();
    let root = demo_wc_root();
    let wc = WcRef {
        id: WcId("demo".into()),
        root: root.clone(),
    };
    let ctx = OpContext::detached();
    let info = engine.wc_info(&root, &ctx).await.unwrap();
    assert_eq!(info.base_rev, Rev(8));
    let st = engine
        .status(
            &wc,
            StatusQuery {
                include_ignored: false,
                include_unversioned: true,
            },
            &ctx,
        )
        .await
        .unwrap();
    let text = |p: &str| st.iter().find(|s| s.path == p).map(|s| s.text);
    assert_eq!(text("README.md"), Some(NodeStatus::Modified));
    assert_eq!(text("notes.txt"), Some(NodeStatus::Unversioned));
    assert_eq!(text("src/util.rs"), Some(NodeStatus::Added));
    assert_eq!(text("docs/guide.md"), Some(NodeStatus::Missing));
    assert!(
        st.iter()
            .any(|s| s.path == "src/lib.rs" && s.lock.is_some())
    );
    assert!(st.iter().any(|s| s.changelist.as_deref() == Some("docs")));
}

#[tokio::test]
async fn commit_leaves_a_mixed_revision_working_copy() {
    let h = harness();
    let root = work_dir().join("mixed");
    let wc = WcRef {
        id: WcId("m".into()),
        root: root.clone(),
    };
    let ctx = OpContext::detached();
    h.engine
        .checkout(&checkout_req(&root, None), &ctx)
        .await
        .unwrap();
    h.engine
        .edit_file(&root, "README.md", b"changed\n")
        .unwrap();
    h.engine
        .commit(
            &wc,
            &CommitRequest {
                wc_id: wc.id.clone(),
                paths: vec!["README.md".into()],
                message: "m".into(),
                keep_locks: false,
                auto_add_remove: true,
            },
            &ctx,
        )
        .await
        .unwrap();
    let info = h.engine.wc_info(&root, &ctx).await.unwrap();
    assert_eq!(info.rev_range, Some((Rev(1), Rev(2))));
}

#[tokio::test]
async fn invalid_paths_and_requests_are_rejected() {
    let h = harness();
    let root = work_dir().join("invalid");
    let wc = WcRef {
        id: WcId("i".into()),
        root: root.clone(),
    };
    let ctx = OpContext::detached();
    h.engine
        .checkout(&checkout_req(&root, None), &ctx)
        .await
        .unwrap();
    for bad in ["../outside", "/abs", "a/../../b"] {
        let err = h
            .engine
            .add_paths(&wc, &[bad.to_owned()], &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsafePath, "{bad}");
    }
    let relative = h
        .engine
        .checkout(&checkout_req(Path::new("relative/path"), None), &ctx)
        .await
        .unwrap_err();
    assert_eq!(relative.code, ErrorCode::Internal);
    let twice = h
        .engine
        .checkout(&checkout_req(&root, None), &ctx)
        .await
        .unwrap_err();
    assert_eq!(twice.code, ErrorCode::Internal);
    h.engine
        .cleanup(
            &wc,
            &CleanupRequest {
                wc_id: wc.id.clone(),
                remove_unversioned: Some(true),
                remove_ignored: None,
                vacuum_pristines: None,
            },
            &ctx,
        )
        .await
        .unwrap();
}

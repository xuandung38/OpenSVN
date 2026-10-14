//! Checkout, update and switch through `svn-client`, on sessions anchored below the repository
//! root (`FakeRaSession::at`), the way the HTTP connector reparents its root session.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use opensvn_ipc::{Depth, Rev, SparseDepth, SparseEntry, UpdateRequest, WcFormat, WcId};
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_ra_api::{
    CommitEditor, CommitObserver, FileMeta, FileRevHandler, LogRequest, RaSession, ReportFn,
};
use svn_types::{
    CancelToken, DeltaEditor, DirEntry, ErrorCode, LockInfo, LogEntry, NodeKind, PropChange,
    RelPath, RepoUrl, Revnum, SvnError,
};
use tokio::io::AsyncWrite;

use super::support::{StaticConnector, URL, UUID, ctx, dir, fixture_on, put, standard_repo};
use crate::engine::context::{OpContext, ProgressReporter, ProgressUpdate};
use crate::engine::facade::{CheckoutPlan, EngineFacade, WcRef};
use crate::engine::real::{Anchor, ClientRecovery, ConnectOptions, SessionConnector};

/// Runs once, right after the next update drive finished (a commit landing between drives).
type AfterUpdate = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

/// Serves `repo`, anchored at the requested URL when asked to; `hang` makes every update drive
/// wait for cancellation, like a server that has not answered the REPORT yet.
struct AnchoringConnector {
    repo: FakeRaSession,
    hang: Arc<AtomicBool>,
    after_update: AfterUpdate,
}

#[async_trait]
impl SessionConnector for AnchoringConnector {
    async fn connect(
        &self,
        url: &RepoUrl,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, SvnError> {
        StaticConnector(vec![self.repo.clone()])
            .connect(url, cancel)
            .await
    }

    async fn connect_with(
        &self,
        url: &RepoUrl,
        options: &ConnectOptions,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, SvnError> {
        cancel.check()?;
        let inner = match options.anchor {
            Anchor::Root => self.repo.clone(),
            Anchor::Url => self.repo.at(url)?,
        };
        Ok(Arc::new(GatedSession {
            inner,
            hang: Arc::clone(&self.hang),
            after_update: Arc::clone(&self.after_update),
        }))
    }
}

struct Setup {
    f: super::support::Fixture,
    hang: Arc<AtomicBool>,
    after_update: AfterUpdate,
    tmp: tempfile::TempDir,
}

fn setup() -> Setup {
    let repo = standard_repo();
    repo.seed(
        "bob",
        "branch",
        &[
            dir("branches/b1"),
            put("branches/b1/README.md", "branch\n"),
            put("branches/b1/src/main.rs", "fn main() { b1() }\n"),
        ],
    )
    .unwrap();
    let hang = Arc::new(AtomicBool::new(false));
    let after_update: AfterUpdate = Arc::new(Mutex::new(None));
    let connector = Arc::new(AnchoringConnector {
        repo: repo.clone(),
        hang: Arc::clone(&hang),
        after_update: Arc::clone(&after_update),
    });
    Setup {
        f: fixture_on(repo, connector, Arc::new(ClientRecovery)),
        hang,
        after_update,
        tmp: tempfile::tempdir().unwrap(),
    }
}

fn plan(url: &str, path: &Path) -> CheckoutPlan {
    CheckoutPlan {
        url: url.to_owned(),
        path: path.to_path_buf(),
        rev: None,
        depth: Depth::Infinity,
        sparse: None,
        format: WcFormat::V31,
        store_pristine: true,
        ignore_externals: false,
    }
}

fn wc_ref(root: &Path) -> WcRef {
    WcRef {
        id: WcId("wc".into()),
        root: root.to_path_buf(),
    }
}

fn update_all() -> UpdateRequest {
    UpdateRequest {
        wc_id: WcId("wc".into()),
        paths: None,
        rev: None,
        depth: None,
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// Context whose progress samples are collected.
fn recording_ctx() -> (OpContext, Arc<Mutex<Vec<ProgressUpdate>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let ctx = OpContext {
        progress: ProgressReporter::new(move |u| sink.lock().unwrap().push(u)),
        ..OpContext::detached()
    };
    (ctx, seen)
}

fn assert_monotonic(samples: &[ProgressUpdate], phase: &str) {
    // Stage samples (connecting, fetching, finishing) carry no count; the path samples do.
    let stages: Vec<_> = samples.iter().filter_map(|u| u.stage).collect();
    assert!(
        stages.is_empty() || stages.first() == Some(&opensvn_ipc::OpStage::Connecting),
        "stages start with connecting: {stages:?}"
    );
    let samples: Vec<_> = samples
        .iter()
        .filter(|u| u.entry.is_some())
        .cloned()
        .collect();
    assert!(!samples.is_empty(), "no progress reported");
    let currents: Vec<u64> = samples.iter().map(|u| u.current.unwrap()).collect();
    let expected: Vec<u64> = (1..=samples.len() as u64).collect();
    assert_eq!(currents, expected, "progress is not monotonic");
    assert!(samples.iter().all(|u| u.phase == phase));
}

#[tokio::test(flavor = "multi_thread")]
async fn checkout_then_update_follows_the_repository() {
    let s = setup();
    let wc = s.tmp.path().join("wc");
    let (c, seen) = recording_ctx();
    let info =
        s.f.engine
            .checkout(&plan(&format!("{URL}/trunk"), &wc), &c)
            .await
            .unwrap();
    assert_eq!(info.url, format!("{URL}/trunk"));
    assert_eq!(info.repo_root_url, URL);
    assert_eq!(info.repo_uuid, UUID);
    assert_eq!(info.path_in_repo, "/trunk");
    assert_eq!(info.base_rev, Rev(2));
    assert_eq!(info.format, WcFormat::V31);
    assert_eq!(read(&wc.join("README.md")), "hello\n");
    assert_eq!(read(&wc.join("src/main.rs")), "fn main() {}\n");
    assert!(!wc.join("trunk").exists() && !wc.join("branches").exists());
    {
        let seen = seen.lock().unwrap();
        assert_monotonic(&seen, "checkout");
        let paths: Vec<_> = seen.iter().filter_map(|u| u.current_path.clone()).collect();
        assert!(paths.contains(&"README.md".to_owned()), "{paths:?}");
        assert!(paths.contains(&"src/main.rs".to_owned()), "{paths:?}");
    }

    // r3 changes trunk and something outside it.
    s.f.repo
        .seed(
            "carol",
            "r3",
            &[
                put("trunk/README.md", "hello again\n"),
                put("trunk/new.txt", "new\n"),
                put("branches/b1/README.md", "elsewhere\n"),
            ],
        )
        .unwrap();
    let (c, seen) = recording_ctx();
    let rev =
        s.f.engine
            .update(&wc_ref(&wc), &update_all(), &c)
            .await
            .unwrap();
    assert_eq!(rev, Rev(3));
    assert_eq!(read(&wc.join("README.md")), "hello again\n");
    assert_eq!(read(&wc.join("new.txt")), "new\n");
    assert_monotonic(&seen.lock().unwrap(), "update");

    // Several paths reach the same revision; progress keeps counting across them.
    s.f.repo
        .seed(
            "carol",
            "r4",
            &[
                put("trunk/README.md", "r4\n"),
                put("trunk/src/main.rs", "fn main() { r4() }\n"),
            ],
        )
        .unwrap();
    let (c, seen) = recording_ctx();
    let req = UpdateRequest {
        paths: Some(vec!["README.md".into(), "src".into()]),
        ..update_all()
    };
    let rev = s.f.engine.update(&wc_ref(&wc), &req, &c).await.unwrap();
    assert_eq!(rev, Rev(4));
    assert_eq!(read(&wc.join("README.md")), "r4\n");
    assert_eq!(read(&wc.join("src/main.rs")), "fn main() { r4() }\n");
    assert_monotonic(&seen.lock().unwrap(), "update");
    let info = s.f.engine.wc_info(&wc, &ctx()).await.unwrap();
    assert_eq!(info.rev_range, Some((Rev(3), Rev(4))));

    // Back to an older revision.
    let req = UpdateRequest {
        rev: Some(Rev(1)),
        ..update_all()
    };
    assert_eq!(
        s.f.engine.update(&wc_ref(&wc), &req, &ctx()).await.unwrap(),
        Rev(1)
    );
    assert_eq!(read(&wc.join("README.md")), "hello\n");
    assert!(!wc.join("new.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn switch_moves_the_working_copy_within_the_repository_only() {
    let s = setup();
    let wc = s.tmp.path().join("wc");
    s.f.engine
        .checkout(&plan(&format!("{URL}/trunk"), &wc), &ctx())
        .await
        .unwrap();

    let rev =
        s.f.engine
            .switch(&wc_ref(&wc), &format!("{URL}/branches/b1"), None, &ctx())
            .await
            .unwrap();
    assert_eq!(rev, Rev(2));
    assert_eq!(read(&wc.join("README.md")), "branch\n");
    assert_eq!(read(&wc.join("src/main.rs")), "fn main() { b1() }\n");
    let info = s.f.engine.wc_info(&wc, &ctx()).await.unwrap();
    assert_eq!(info.url, format!("{URL}/branches/b1"));
    assert_eq!(info.path_in_repo, "/branches/b1");

    // An update now follows the branch.
    s.f.repo
        .seed("bob", "r3", &[put("branches/b1/README.md", "branch r3\n")])
        .unwrap();
    s.f.engine
        .update(&wc_ref(&wc), &update_all(), &ctx())
        .await
        .unwrap();
    assert_eq!(read(&wc.join("README.md")), "branch r3\n");

    // Back to trunk at a fixed revision.
    let rev =
        s.f.engine
            .switch(&wc_ref(&wc), &format!("{URL}/trunk"), Some(Rev(1)), &ctx())
            .await
            .unwrap();
    assert_eq!(rev, Rev(1));
    assert_eq!(read(&wc.join("README.md")), "hello\n");

    for other in [
        "https://svn.test.local/repos/other/trunk",
        "https://svn.test.local/repos/realx/trunk",
        "https://elsewhere.test.local/repos/real/trunk",
    ] {
        let err =
            s.f.engine
                .switch(&wc_ref(&wc), other, None, &ctx())
                .await
                .unwrap_err();
        assert_eq!(err.code, ErrorCode::WcInvalidOperation, "{other}: {err}");
        assert!(err.message.contains("repository"), "{err}");
    }
    assert_eq!(read(&wc.join("README.md")), "hello\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_while_waiting_for_the_server_is_fast_and_update_resumes_the_checkout() {
    let s = setup();
    let wc = s.tmp.path().join("wc");
    s.hang.store(true, Ordering::SeqCst);
    let c = ctx();
    let engine_task = {
        let (c, p) = (c.clone(), plan(&format!("{URL}/trunk"), &wc));
        let f = &s.f;
        async move { f.engine.checkout(&p, &c).await }
    };
    let canceller = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let at = Instant::now();
        c.cancel.cancel();
        at
    };
    let (result, cancelled_at) = tokio::join!(engine_task, canceller);
    let elapsed = cancelled_at.elapsed();
    assert_eq!(result.unwrap_err().code, ErrorCode::Cancelled);
    assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
    assert!(!wc.join("README.md").exists());

    // The interrupted checkout is a working copy that an update completes.
    s.hang.store(false, Ordering::SeqCst);
    let rev =
        s.f.engine
            .update(&wc_ref(&wc), &update_all(), &ctx())
            .await
            .unwrap();
    assert_eq!(rev, Rev(2));
    assert_eq!(read(&wc.join("README.md")), "hello\n");
    assert_eq!(read(&wc.join("src/main.rs")), "fn main() {}\n");
    let info = s.f.engine.wc_info(&wc, &ctx()).await.unwrap();
    assert_eq!((info.base_rev, info.rev_range), (Rev(2), None));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checkout_cancelled_half_way_is_finished_by_update() {
    let s = setup();
    let wc = s.tmp.path().join("wc");
    let cancel = CancelToken::new();
    let trip = cancel.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let c = OpContext {
        cancel: cancel.clone(),
        progress: ProgressReporter::new(move |u: ProgressUpdate| {
            let path = u.entry.is_some();
            sink.lock().unwrap().push(u);
            if path {
                trip.cancel(); // first path applied: the user cancels
            }
        }),
        interactive: true,
    };
    let err =
        s.f.engine
            .checkout(&plan(&format!("{URL}/trunk"), &wc), &c)
            .await
            .unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(!seen.lock().unwrap().is_empty());
    assert!(
        !(wc.join("README.md").exists() && wc.join("src/main.rs").exists()),
        "the checkout was not interrupted"
    );
    let info = s.f.engine.wc_info(&wc, &ctx()).await.unwrap();
    assert!(
        info.incomplete,
        "an interrupted checkout is reported as incomplete"
    );

    let rev =
        s.f.engine
            .update(&wc_ref(&wc), &update_all(), &ctx())
            .await
            .unwrap();
    assert_eq!(rev, Rev(2));
    assert_eq!(read(&wc.join("README.md")), "hello\n");
    assert_eq!(read(&wc.join("src/main.rs")), "fn main() {}\n");
    assert!(!s.f.engine.wc_info(&wc, &ctx()).await.unwrap().incomplete);
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_requests_fail_with_user_facing_errors() {
    let s = setup();
    let wc = s.tmp.path().join("wc");
    let url = format!("{URL}/trunk");

    let err =
        s.f.engine
            .checkout(
                &CheckoutPlan {
                    format: WcFormat::V32,
                    ..plan(&url, &wc)
                },
                &ctx(),
            )
            .await
            .unwrap_err();
    assert_eq!(err.code, ErrorCode::WcFormatUnsupported);

    // A pristine-less working copy is refused, never silently created with pristine copies.
    let err =
        s.f.engine
            .checkout(
                &CheckoutPlan {
                    store_pristine: false,
                    ..plan(&url, &wc)
                },
                &ctx(),
            )
            .await
            .unwrap_err();
    assert_eq!(err.code, ErrorCode::WcFormatUnsupported);
    assert!(err.message.contains("pristine"), "{err}");
    assert!(!wc.exists(), "a refused checkout must not create anything");

    let err =
        s.f.engine
            .checkout(
                &CheckoutPlan {
                    sparse: Some(vec![SparseEntry {
                        path: "src".into(),
                        depth: SparseDepth::Empty,
                    }]),
                    ..plan(&url, &wc)
                },
                &ctx(),
            )
            .await
            .unwrap_err();
    assert_eq!(err.code, ErrorCode::WcInvalidOperation);
    assert!(err.message.contains("sparse"), "{err}");
    assert!(!wc.exists(), "a refused checkout must not create anything");

    s.f.engine.checkout(&plan(&url, &wc), &ctx()).await.unwrap();
    let err =
        s.f.engine
            .set_depth(&wc_ref(&wc), "README.md", Depth::Empty, &ctx())
            .await
            .unwrap_err();
    assert_eq!(
        err.code,
        ErrorCode::WcInvalidOperation,
        "a file has no depth"
    );
    assert!(err.message.contains("depth"), "{err}");

    let err =
        s.f.engine
            .update(
                &wc_ref(&wc),
                &UpdateRequest {
                    paths: Some(vec!["../outside".into()]),
                    ..update_all()
                },
                &ctx(),
            )
            .await
            .unwrap_err();
    assert_ne!(err.code, ErrorCode::Internal, "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_commit_between_paths_leaves_every_path_at_the_same_revision() {
    let s = setup();
    let wc = s.tmp.path().join("wc");
    s.f.engine
        .checkout(&plan(&format!("{URL}/trunk"), &wc), &ctx())
        .await
        .unwrap();
    s.f.repo
        .seed(
            "carol",
            "r3",
            &[
                put("trunk/README.md", "r3\n"),
                put("trunk/src/main.rs", "fn main() { r3() }\n"),
            ],
        )
        .unwrap();
    // r4 lands after the first path was updated and before the second one is.
    let repo = s.f.repo.clone();
    *s.after_update.lock().unwrap() = Some(Box::new(move || {
        repo.seed(
            "dave",
            "r4",
            &[
                put("trunk/README.md", "r4\n"),
                put("trunk/src/main.rs", "fn main() { r4() }\n"),
            ],
        )
        .unwrap();
    }));
    let req = UpdateRequest {
        paths: Some(vec!["README.md".into(), "src".into()]),
        ..update_all()
    };
    let rev = s.f.engine.update(&wc_ref(&wc), &req, &ctx()).await.unwrap();
    assert!(
        s.after_update.lock().unwrap().is_none(),
        "r4 was not committed"
    );
    assert_eq!(rev, Rev(3));
    assert_eq!(read(&wc.join("README.md")), "r3\n");
    assert_eq!(read(&wc.join("src/main.rs")), "fn main() { r3() }\n");
    // The root was not updated (r2); both updated paths are at r3 and nothing reached r4.
    let info = s.f.engine.wc_info(&wc, &ctx()).await.unwrap();
    assert_eq!(info.rev_range, Some((Rev(2), Rev(3))));
}

fn set_trunk_externals(s: &Setup, value: &str) {
    s.f.repo
        .seed(
            "carol",
            "externals",
            &[FakeChange::SetProp(
                RelPath::new("trunk").unwrap(),
                PropChange {
                    name: "svn:externals".into(),
                    value: Some(Bytes::from(value.to_owned())),
                },
            )],
        )
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn checkout_and_update_fetch_externals_unless_ignored() {
    let s = setup();
    assert!(s.f.engine.capabilities().externals);
    set_trunk_externals(
        &s,
        "^/branches/b1 ext\n^/branches/b1/README.md readme-ext\n",
    );
    for ignore_externals in [false, true] {
        let wc = s.tmp.path().join(format!("wc-{ignore_externals}"));
        s.f.engine
            .checkout(
                &CheckoutPlan {
                    ignore_externals,
                    ..plan(&format!("{URL}/trunk"), &wc)
                },
                &ctx(),
            )
            .await
            .unwrap();
        assert_eq!(wc.join("ext").exists(), !ignore_externals);
        assert_eq!(wc.join("readme-ext").exists(), !ignore_externals);
        if !ignore_externals {
            assert_eq!(read(&wc.join("ext/README.md")), "branch\n");
            assert_eq!(read(&wc.join("readme-ext")), "branch\n");
        }
    }
    // An update follows the changed definition: the old external goes, a new one comes.
    let wc = s.tmp.path().join("wc-false");
    set_trunk_externals(&s, "^/branches/b1/src moved\n");
    s.f.engine
        .update(&wc_ref(&wc), &update_all(), &ctx())
        .await
        .unwrap();
    assert!(!wc.join("ext").exists());
    assert!(!wc.join("readme-ext").exists());
    assert_eq!(read(&wc.join("moved/main.rs")), "fn main() { b1() }\n");
    // Status reports the external as `X`, never as unversioned.
    let st =
        s.f.engine
            .status(
                &wc_ref(&wc),
                crate::engine::facade::StatusQuery {
                    include_ignored: false,
                    include_unversioned: true,
                },
                &ctx(),
            )
            .await
            .unwrap();
    let moved = st.iter().find(|f| f.path == "moved").expect("listed");
    assert_eq!(moved.text, opensvn_ipc::NodeStatus::External, "{st:?}");
}

/// Serves `inner`, but refuses every URL on `refused_host` the way the transport refuses an
/// untrusted certificate.
struct RefusingConnector {
    inner: AnchoringConnector,
    refused_host: &'static str,
    asked: Mutex<Vec<String>>,
}

#[async_trait]
impl SessionConnector for RefusingConnector {
    async fn connect(
        &self,
        url: &RepoUrl,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, SvnError> {
        self.inner.connect(url, cancel).await
    }

    async fn connect_with(
        &self,
        url: &RepoUrl,
        options: &ConnectOptions,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, SvnError> {
        self.asked.lock().unwrap().push(url.to_string());
        if url.as_url().host_str() == Some(self.refused_host) {
            assert_eq!(options.anchor, Anchor::Url);
            return Err(SvnError::new(
                ErrorCode::TlsUntrusted,
                "the server certificate is not trusted",
            ));
        }
        self.inner.connect_with(url, options, cancel).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_external_on_another_host_goes_through_the_connector_and_only_warns() {
    let repo = standard_repo();
    repo.seed(
        "bob",
        "branch",
        &[
            put("branches/b1/README.md", "branch\n"),
            FakeChange::SetProp(
                RelPath::new("trunk").unwrap(),
                PropChange {
                    name: "svn:externals".into(),
                    value: Some(Bytes::from_static(
                        b"https://evil.example/svn/x far\n^/branches/b1 near\n",
                    )),
                },
            ),
        ],
    )
    .unwrap();
    let connector = Arc::new(RefusingConnector {
        inner: AnchoringConnector {
            repo: repo.clone(),
            hang: Arc::new(AtomicBool::new(false)),
            after_update: Arc::new(Mutex::new(None)),
        },
        refused_host: "evil.example",
        asked: Mutex::new(Vec::new()),
    });
    let f = fixture_on(repo, Arc::clone(&connector) as _, Arc::new(ClientRecovery));
    let tmp = tempfile::tempdir().unwrap();
    let wc = tmp.path().join("wc");
    let (ctx, seen) = recording_ctx();
    f.engine
        .checkout(&plan(&format!("{URL}/trunk"), &wc), &ctx)
        .await
        .unwrap();
    assert!(!wc.join("far").exists());
    assert_eq!(read(&wc.join("near/README.md")), "branch\n");
    assert!(
        connector
            .asked
            .lock()
            .unwrap()
            .iter()
            .any(|u| u.starts_with("https://evil.example/")),
        "the other host was asked through the connector"
    );
    let warnings: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|u| u.phase == "external-warning")
        .map(|u| u.current_path.clone())
        .collect();
    assert_eq!(warnings, [Some("far".to_owned())]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connector_that_cannot_anchor_is_refused_instead_of_checking_out_the_root() {
    let repo = standard_repo();
    let f = fixture_on(
        repo.clone(),
        Arc::new(StaticConnector(vec![repo])),
        Arc::new(ClientRecovery),
    );
    let tmp = tempfile::tempdir().unwrap();
    let wc = tmp.path().join("wc");
    let err = f
        .engine
        .checkout(&plan(&format!("{URL}/trunk"), &wc), &ctx())
        .await
        .unwrap_err();
    assert!(err.message.contains("instead of"), "{err}");
    assert!(!wc.exists());
}

#[tokio::test]
async fn cancelled_contexts_do_nothing() {
    let s = setup();
    let wc = s.tmp.path().join("wc");
    let c = ctx();
    c.cancel.cancel();
    let err =
        s.f.engine
            .checkout(&plan(&format!("{URL}/trunk"), &wc), &c)
            .await
            .unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
    let err =
        s.f.engine
            .update(&wc_ref(&wc), &update_all(), &c)
            .await
            .unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(!wc.exists());
}

/// The in-memory repository, except that update drives can be held until cancellation.
struct GatedSession {
    inner: FakeRaSession,
    hang: Arc<AtomicBool>,
    after_update: AfterUpdate,
}

#[async_trait]
impl RaSession for GatedSession {
    fn repos_root(&self) -> &RepoUrl {
        self.inner.repos_root()
    }
    fn uuid(&self) -> &str {
        self.inner.uuid()
    }
    fn session_url(&self) -> &RepoUrl {
        self.inner.session_url()
    }
    async fn ensure_authenticated(&self, cancel: &CancelToken) -> Result<(), SvnError> {
        self.inner.ensure_authenticated(cancel).await
    }
    async fn youngest_rev(&self, cancel: &CancelToken) -> Result<Revnum, SvnError> {
        self.inner.youngest_rev(cancel).await
    }
    async fn check_path(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<NodeKind, SvnError> {
        self.inner.check_path(path, rev, cancel).await
    }
    async fn stat(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<Option<DirEntry>, SvnError> {
        self.inner.stat(path, rev, cancel).await
    }
    async fn list(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        depth: svn_types::Depth,
        cancel: &CancelToken,
    ) -> Result<Vec<DirEntry>, SvnError> {
        self.inner.list(path, rev, depth, cancel).await
    }
    async fn get_file(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        sink: &mut (dyn AsyncWrite + Send + Unpin),
        cancel: &CancelToken,
    ) -> Result<FileMeta, SvnError> {
        self.inner.get_file(path, rev, sink, cancel).await
    }
    async fn log(
        &self,
        req: &LogRequest,
        on_entry: &mut (dyn FnMut(LogEntry) -> Result<(), SvnError> + Send),
        cancel: &CancelToken,
    ) -> Result<(), SvnError> {
        self.inner.log(req, on_entry, cancel).await
    }
    async fn get_locks(
        &self,
        path: &RelPath,
        depth: svn_types::Depth,
        cancel: &CancelToken,
    ) -> Result<Vec<LockInfo>, SvnError> {
        self.inner.get_locks(path, depth, cancel).await
    }
    async fn lock(
        &self,
        path: &RelPath,
        comment: Option<&str>,
        steal: bool,
        base_rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<LockInfo, SvnError> {
        self.inner
            .lock(path, comment, steal, base_rev, cancel)
            .await
    }
    async fn unlock(
        &self,
        path: &RelPath,
        token: Option<&str>,
        break_lock: bool,
        cancel: &CancelToken,
    ) -> Result<(), SvnError> {
        self.inner.unlock(path, token, break_lock, cancel).await
    }
    async fn file_revs(
        &self,
        path: &RelPath,
        start: Option<Revnum>,
        end: Option<Revnum>,
        handler: &mut dyn FileRevHandler,
        cancel: &CancelToken,
    ) -> Result<(), SvnError> {
        self.inner
            .file_revs(path, start, end, handler, cancel)
            .await
    }
    async fn do_update(
        &self,
        req: svn_ra_api::UpdateRequest,
        report: ReportFn,
        editor: Box<dyn DeltaEditor>,
        cancel: &CancelToken,
    ) -> Result<(), SvnError> {
        if self.hang.load(Ordering::SeqCst) {
            cancel.cancelled().await;
            return Err(crate::engine::context::cancelled());
        }
        self.inner.do_update(req, report, editor, cancel).await?;
        let then = self.after_update.lock().unwrap().take();
        if let Some(then) = then {
            then();
        }
        Ok(())
    }
    async fn commit_editor(
        &self,
        message: &str,
        rev_props: &[PropChange],
        lock_tokens: &[(String, String)],
        keep_locks: bool,
        observer: Arc<dyn CommitObserver>,
        cancel: &CancelToken,
    ) -> Result<Box<dyn CommitEditor>, SvnError> {
        self.inner
            .commit_editor(
                message,
                rev_props,
                lock_tokens,
                keep_locks,
                observer,
                cancel,
            )
            .await
    }
    async fn abort_txn(&self, txn_name: &str) -> Result<(), SvnError> {
        self.inner.abort_txn(txn_name).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn set_depth_crops_without_losing_local_changes_and_widens_again() {
    let s = setup();
    let wc = s.tmp.path().join("wc");
    let url = format!("{URL}/trunk");
    s.f.engine.checkout(&plan(&url, &wc), &ctx()).await.unwrap();
    std::fs::write(wc.join("src/notes.txt"), "mine\n").unwrap();
    let wcr = wc_ref(&wc);

    s.f.engine
        .set_depth(&wcr, "src", Depth::Empty, &ctx())
        .await
        .unwrap();
    assert!(!wc.join("src/main.rs").exists(), "unmodified files go");
    assert_eq!(
        read(&wc.join("src/notes.txt")),
        "mine\n",
        "unversioned files stay"
    );
    let db = svn_wc::WcDb::open(&wc).unwrap();
    let src = RelPath::new("src").unwrap();
    assert_eq!(
        db.read_base(&src).unwrap().unwrap().depth,
        Some(svn_types::Depth::Empty)
    );
    assert!(
        db.read_base(&RelPath::new("src/main.rs").unwrap())
            .unwrap()
            .is_none()
    );

    // A locally modified file is never removed: it stays versioned and modified.
    s.f.engine
        .set_depth(&wcr, "src", Depth::Infinity, &ctx())
        .await
        .unwrap();
    assert_eq!(read(&wc.join("src/main.rs")), "fn main() {}\n");
    std::fs::write(wc.join("src/main.rs"), "fn main() { local(); }\n").unwrap();
    s.f.engine
        .set_depth(&wcr, "", Depth::Empty, &ctx())
        .await
        .unwrap();
    assert_eq!(read(&wc.join("src/main.rs")), "fn main() { local(); }\n");
    assert!(!wc.join("README.md").exists());
    assert!(
        db.read_base(&RelPath::new("src/main.rs").unwrap())
            .unwrap()
            .is_some(),
        "the modified file stays versioned"
    );
    s.f.engine
        .set_depth(&wcr, "", Depth::Infinity, &ctx())
        .await
        .unwrap();
    assert_eq!(read(&wc.join("README.md")), "hello\n");
    assert_eq!(read(&wc.join("src/main.rs")), "fn main() { local(); }\n");
}

/// A checked-out `svn:needs-lock` file is read-only, writable while locked and read-only
/// again after the unlock (`svn lock` / `svn unlock`).
#[tokio::test]
async fn locking_a_needs_lock_file_makes_it_writable_until_unlocked() {
    use crate::engine::facade::LockScope;

    let s = setup();
    s.f.repo
        .seed(
            "alice",
            "needs-lock",
            &[FakeChange::SetProp(
                RelPath::new("trunk/README.md").unwrap(),
                PropChange {
                    name: "svn:needs-lock".into(),
                    value: Some(Bytes::from_static(b"*")),
                },
            )],
        )
        .unwrap();
    let wc = s.tmp.path().join("wc");
    s.f.engine
        .checkout(&plan(&format!("{URL}/trunk"), &wc), &ctx())
        .await
        .unwrap();
    let read_only = || {
        std::fs::metadata(wc.join("README.md"))
            .unwrap()
            .permissions()
            .readonly()
    };
    assert!(read_only(), "read-only after checkout");
    let scope = LockScope::Wc {
        wc: wc_ref(&wc),
        paths: vec!["README.md".into()],
    };
    s.f.engine.lock(&scope, None, false, &ctx()).await.unwrap();
    assert!(!read_only(), "writable while locked");
    s.f.engine.unlock(&scope, false, &ctx()).await.unwrap();
    assert!(read_only(), "read-only again after the unlock");
}

//! Shared helpers for the integration tests.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used, clippy::panic)]

pub mod oracle;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use svn_client::ClientCtx;
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_types::{MemoryLockOwnership, PropChange, RelPath};
use svn_wc::db::{LockOwner, WcDb};

pub const ROOT_URL: &str = "https://svn.test.local/repos/demo";

pub fn rp(s: &str) -> RelPath {
    RelPath::new(s).expect("relpath")
}

pub fn put(path: &str, content: &str) -> FakeChange {
    FakeChange::PutFile(rp(path), Bytes::from(content.to_owned()))
}

pub fn dir(path: &str) -> FakeChange {
    FakeChange::PutDir(rp(path))
}

pub fn del(path: &str) -> FakeChange {
    FakeChange::Delete(rp(path))
}

pub fn prop(path: &str, name: &str, value: Option<&str>) -> FakeChange {
    FakeChange::SetProp(
        rp(path),
        PropChange {
            name: name.to_owned(),
            value: value.map(|v| Bytes::from(v.to_owned())),
        },
    )
}

pub fn ctx() -> ClientCtx {
    ClientCtx::new(LockOwner {
        store: Arc::new(MemoryLockOwnership::for_current_process("test-boot")),
        pid: std::process::id(),
        boot_id: "test-boot".to_owned(),
    })
}

pub fn session() -> FakeRaSession {
    FakeRaSession::new(ROOT_URL, "uuid-demo").expect("session")
}

pub struct Sandbox {
    pub dir: tempfile::TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().canonicalize().expect("canon").join(name)
    }
}

pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("read")
}

pub fn open_db(wc: &Path) -> WcDb {
    WcDb::open(wc).expect("open wc.db")
}

/// All BASE rows as `(relpath, presence, kind, revision, repos_path)` sorted by path.
pub fn base_rows(db: &WcDb) -> Vec<(String, String, String, i64, String)> {
    db.with_read(|c| {
        let mut stmt = c
            .prepare(
                "SELECT local_relpath, presence, kind, revision, COALESCE(repos_path, '') \
                 FROM nodes WHERE op_depth = 0 ORDER BY local_relpath",
            )
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .expect("query");
        Ok(rows.map(|r| r.expect("row")).collect())
    })
    .expect("read")
}

/// Opens sessions for externals over a set of fake repositories (by repository root), and
/// records every URL it was asked for.
pub struct FakeOpener {
    pub repos: Vec<FakeRaSession>,
    pub opened: std::sync::Mutex<Vec<String>>,
}

impl FakeOpener {
    pub fn new(repos: Vec<FakeRaSession>) -> Arc<Self> {
        Arc::new(Self {
            repos,
            opened: std::sync::Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl svn_client::externals::SessionOpener for FakeOpener {
    async fn open(
        &self,
        url: &svn_types::RepoUrl,
        _cancel: &svn_types::CancelToken,
    ) -> svn_types::Result<Arc<dyn svn_ra_api::RaSession>> {
        use svn_ra_api::RaSession as _;
        self.opened.lock().unwrap().push(url.to_string());
        for r in &self.repos {
            if let Ok(s) = r.at(url) {
                return Ok(Arc::new(s));
            }
            let _ = r.repos_root();
        }
        Err(svn_types::SvnError::new(
            svn_types::ErrorCode::Network,
            format!("no fake repository serves {url}"),
        ))
    }
}

/// [`ctx`] that fetches externals through `opener`.
pub fn ext_ctx(opener: Arc<FakeOpener>) -> ClientCtx {
    let mut c = ctx();
    c.externals = Some(opener);
    c
}

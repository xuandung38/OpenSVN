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
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .expect("query");
        Ok(rows.map(|r| r.expect("row")).collect())
    })
    .expect("read")
}

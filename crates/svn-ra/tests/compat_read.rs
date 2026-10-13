//! Read operations of `HttpRaSession` against a real mod_dav_svn, with the `svn` command line
//! as oracle.
//!
//! Ignored by default. Point `OPENSVN_DAV_URL` at the root URL of a scratch repository that
//! allows anonymous read and write over plain HTTP (for example Apache with
//! `DAV svn` + `SVNParentPath` + `Require all granted` on 127.0.0.1) and that has a `trunk`
//! with at least one file and one subdirectory, then run
//! `cargo test -p svn-ra --test compat_read -- --ignored --test-threads=1`.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod ra_write_support;

use std::collections::BTreeSet;
use std::process::Command;
use std::sync::Arc;

use md5::{Digest, Md5};
use ra_write_support::{FixedPrompter, transport};
use svn_ra::ops::HttpRaSession;
use svn_ra::ops::remote_edit::{RemoteCommit, RemoteOp, commit_ops};
use svn_ra_api::{CommitInfo, CommitObserver, FileRev, FileRevHandler, LogRequest, RaSession};
use svn_types::{
    CancelToken, Depth, NodeKind, RelPath, RepoUrl, Result, Revnum, TxDeltaWindow, WindowSink,
};

fn root_url() -> Option<String> {
    std::env::var("OPENSVN_DAV_URL")
        .ok()
        .map(|u| u.trim_end_matches('/').to_owned())
}

fn svn(args: &[&str]) -> String {
    let out = Command::new("svn")
        .args(["--non-interactive", "--no-auth-cache"])
        .args(args)
        .output()
        .expect("run svn");
    assert!(
        out.status.success(),
        "svn {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8")
}

async fn open(url: &str) -> HttpRaSession {
    HttpRaSession::open(
        transport(Arc::new(FixedPrompter::default())),
        RepoUrl::parse(url).unwrap(),
        &CancelToken::new(),
    )
    .await
    .expect("open session")
}

fn p(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

/// Values of `attr="..."` (or text of `<tag>`) in svn's XML output, in order.
fn xml_values(xml: &str, open: &str, close: &str) -> Vec<String> {
    xml.split(open)
        .skip(1)
        .filter_map(|rest| rest.split(close).next())
        .map(|v| {
            v.replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
        })
        .collect()
}

#[tokio::test]
#[ignore = "needs a mod_dav_svn server: set OPENSVN_DAV_URL"]
async fn session_basics_match_svn_info() {
    let Some(root) = root_url() else { return };
    let trunk = format!("{root}/trunk");
    let s = open(&trunk).await;
    let cancel = CancelToken::new();
    let info = svn(&["info", "--xml", &trunk]);
    assert_eq!(
        xml_values(&info, "<uuid>", "</uuid>"),
        [s.uuid().to_owned()]
    );
    assert_eq!(
        xml_values(&info, "<root>", "</root>"),
        [s.repos_root().as_url().to_string()]
    );
    let head: u64 = svn(&["info", "--show-item", "revision", &root])
        .trim()
        .parse()
        .unwrap();
    assert_eq!(s.youngest_rev(&cancel).await.unwrap(), Revnum(head));
    s.ensure_authenticated(&cancel).await.unwrap();
    assert_eq!(
        s.check_path(&RelPath::root(), None, &cancel).await.unwrap(),
        NodeKind::Dir
    );
    assert_eq!(
        s.check_path(&p("no/such/node"), None, &cancel)
            .await
            .unwrap(),
        NodeKind::None
    );
}

#[tokio::test]
#[ignore = "needs a mod_dav_svn server: set OPENSVN_DAV_URL"]
async fn list_and_stat_match_svn_list() {
    let Some(root) = root_url() else { return };
    let trunk = format!("{root}/trunk");
    let s = open(&trunk).await;
    let cancel = CancelToken::new();
    for (depth, args) in [
        (Depth::Immediates, vec!["list", "--xml"]),
        (Depth::Infinity, vec!["list", "--xml", "-R"]),
    ] {
        let mut a = args.clone();
        a.push(&trunk);
        let xml = svn(&a);
        let expected: BTreeSet<String> =
            xml_values(&xml, "<name>", "</name>").into_iter().collect();
        let ours = s
            .list(&RelPath::root(), None, depth, &cancel)
            .await
            .unwrap();
        let got: BTreeSet<String> = ours.iter().map(|e| e.name.clone()).collect();
        assert_eq!(got, expected, "depth {depth:?}");
        let sizes = xml_values(&xml, "<size>", "</size>");
        let our_sizes: Vec<String> = {
            let mut v: Vec<_> = ours
                .iter()
                .filter_map(|e| e.size)
                .map(|n| n.to_string())
                .collect();
            v.sort();
            v
        };
        let mut sizes = sizes;
        sizes.sort();
        assert_eq!(our_sizes, sizes, "sizes at depth {depth:?}");
        for e in &ours {
            let st = s.stat(&p(&e.name), None, &cancel).await.unwrap().unwrap();
            assert_eq!(st.kind, e.kind);
            assert_eq!(st.created_rev, e.created_rev);
            assert_eq!(st.last_author, e.last_author);
        }
    }
}

#[tokio::test]
#[ignore = "needs a mod_dav_svn server: set OPENSVN_DAV_URL"]
async fn get_file_matches_svn_cat_and_proplist() {
    let Some(root) = root_url() else { return };
    let trunk = format!("{root}/trunk");
    let s = open(&trunk).await;
    let cancel = CancelToken::new();
    let files: Vec<_> = s
        .list(&RelPath::root(), None, Depth::Infinity, &cancel)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == NodeKind::File)
        .collect();
    assert!(!files.is_empty(), "trunk needs at least one file");
    for f in files {
        let url = format!("{trunk}/{}", f.name);
        let mut ours = Vec::new();
        let meta = s
            .get_file(&p(&f.name), None, &mut ours, &cancel)
            .await
            .unwrap();
        let theirs = Command::new("svn")
            .args(["cat", "--non-interactive", &url])
            .output()
            .unwrap()
            .stdout;
        assert_eq!(ours, theirs, "{}", f.name);
        let props = svn(&["proplist", "--xml", &url]);
        let mut expected: Vec<String> = xml_values(&props, "<property\n   name=\"", "\"");
        expected.sort();
        let mut got: Vec<String> = meta.props.iter().map(|c| c.name.clone()).collect();
        got.sort();
        assert_eq!(got, expected, "props of {}", f.name);
    }
}

#[tokio::test]
#[ignore = "needs a mod_dav_svn server: set OPENSVN_DAV_URL"]
async fn log_matches_svn_log() {
    let Some(root) = root_url() else { return };
    let s = open(&root).await;
    let cancel = CancelToken::new();
    for limit in [None, Some(2)] {
        let mut args = vec!["log", "--xml", "-v"];
        let l;
        if let Some(n) = limit {
            l = n.to_string();
            args.extend(["-l", &l]);
        }
        args.push(&root);
        let xml = svn(&args);
        let expected_revs: Vec<u64> = xml_values(&xml, "revision=\"", "\"")
            .iter()
            .map(|r| r.parse().unwrap())
            .collect();
        let req = LogRequest {
            paths: Vec::new(),
            start: None,
            end: None,
            limit,
            discover_changed_paths: true,
            strict_node_history: false,
        };
        let mut entries = Vec::new();
        s.log(
            &req,
            &mut |e| {
                entries.push(e);
                Ok(())
            },
            &cancel,
        )
        .await
        .unwrap();
        let revs: Vec<u64> = entries.iter().map(|e| e.rev.0).collect();
        assert_eq!(revs, expected_revs, "limit {limit:?}");
        let msgs = xml_values(&xml, "<msg>", "</msg>");
        let ours: Vec<String> = entries
            .iter()
            .map(|e| e.message.clone().unwrap_or_default())
            .collect();
        assert_eq!(ours, msgs);
        let paths = xml_values(&xml, "prop-mods=\"", "</path>").len();
        let our_paths: usize = entries.iter().map(|e| e.changed_paths.len()).sum();
        assert_eq!(our_paths, paths);
    }
}

#[tokio::test]
#[ignore = "needs a mod_dav_svn server: set OPENSVN_DAV_URL"]
async fn get_locks_matches_svn_info() {
    let Some(root) = root_url() else { return };
    let s = open(&root).await;
    let locks = s
        .get_locks(&RelPath::root(), Depth::Infinity, &CancelToken::new())
        .await
        .unwrap();
    let xml = svn(&["info", "--xml", "-R", &root]);
    let mut expected = xml_values(&xml, "<token>", "</token>");
    expected.sort();
    let mut got: Vec<String> = locks.iter().map(|l| l.token.clone()).collect();
    got.sort();
    assert_eq!(got, expected);
}

/// Counts revisions and checks the last rebuilt text against `svn cat`.
#[derive(Default)]
struct Texts {
    revs: Vec<Revnum>,
    current: Arc<std::sync::Mutex<Vec<u8>>>,
}

struct Rebuild {
    applier: svn_delta::WindowApplier<std::io::Cursor<Vec<u8>>, Vec<u8>>,
    into: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl WindowSink for Rebuild {
    fn window(&mut self, w: Option<&TxDeltaWindow>) -> Result<()> {
        match w {
            Some(w) => self.applier.apply_window(w),
            None => {
                self.applier.finish()?;
                let out = std::mem::replace(
                    &mut self.applier,
                    svn_delta::WindowApplier::new(std::io::Cursor::new(Vec::new()), Vec::new()),
                )
                .into_output();
                *self.into.lock().unwrap() = out;
                Ok(())
            }
        }
    }
}

impl FileRevHandler for Texts {
    fn revision(&mut self, rev: FileRev) -> Result<Option<Box<dyn WindowSink>>> {
        self.revs.push(rev.rev);
        let base = self.current.lock().unwrap().clone();
        Ok(Some(Box::new(Rebuild {
            applier: svn_delta::WindowApplier::new(std::io::Cursor::new(base), Vec::new()),
            into: Arc::clone(&self.current),
        })))
    }
}

#[tokio::test]
#[ignore = "needs a mod_dav_svn server: set OPENSVN_DAV_URL"]
async fn file_revs_match_svn_blame() {
    let Some(root) = root_url() else { return };
    let trunk = format!("{root}/trunk");
    let s = open(&trunk).await;
    let cancel = CancelToken::new();
    let file = s
        .list(&RelPath::root(), None, Depth::Files, &cancel)
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("a file in trunk");
    let mut h = Texts::default();
    s.file_revs(&p(&file.name), None, None, &mut h, &cancel)
        .await
        .unwrap();
    let url = format!("{trunk}/{}", file.name);
    let blame = svn(&["blame", "--xml", &url]);
    let blame_revs: BTreeSet<u64> = xml_values(&blame, "revision=\"", "\"")
        .iter()
        .map(|r| r.parse().unwrap())
        .collect();
    let ours: BTreeSet<u64> = h.revs.iter().map(|r| r.0).collect();
    assert!(blame_revs.is_subset(&ours), "{blame_revs:?} vs {ours:?}");
    let cat = Command::new("svn")
        .args(["cat", "--non-interactive", &url])
        .output()
        .unwrap()
        .stdout;
    let last = h.current.lock().unwrap().clone();
    assert_eq!(Md5::digest(&last), Md5::digest(&cat));
}

struct NoJournal;

impl CommitObserver for NoJournal {
    fn on_txn_created(&self, _txn: &str) -> Result<()> {
        Ok(())
    }
    fn on_merged(&self, _info: &CommitInfo) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
#[ignore = "needs a mod_dav_svn server: set OPENSVN_DAV_URL"]
async fn remote_mkdir_through_the_session() {
    let Some(root) = root_url() else { return };
    let s = open(&root).await;
    let cancel = CancelToken::new();
    let name = format!(
        "scratch-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let before = s.youngest_rev(&cancel).await.unwrap();
    let info = commit_ops(
        &s,
        &[RemoteOp::Mkdir(p(&name))],
        &RemoteCommit {
            message: "remote mkdir from compat_read".into(),
            rev_props: Vec::new(),
            observer: Arc::new(NoJournal),
        },
        &cancel,
    )
    .await
    .unwrap();
    assert!(info.revision > before);
    assert_eq!(
        s.check_path(&p(&name), None, &cancel).await.unwrap(),
        NodeKind::Dir
    );
    let kind = svn(&["info", "--show-item", "kind", &format!("{root}/{name}")]);
    assert_eq!(kind.trim(), "dir");
}

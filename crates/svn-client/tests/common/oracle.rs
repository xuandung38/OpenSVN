//! Oracle helpers: a real repository driven with svn 1.15, replayed into the fake session.
//!
//! The client under test has no `file://` RA layer, so the repository is created and committed
//! with the real svn, dumped with `svnadmin dump` (full texts, no deltas) and replayed into
//! [`FakeRaSession`] revision by revision. Both clients then work from identical repository
//! history and their working copies are compared row by row and byte by byte.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

use bytes::Bytes;
use opensvn_svn_oracle::{MinVersion, Oracle};
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_types::PropChange;
use svn_wc::db::WcDb;
use svn_wc::skel::{Skel, parse_proplist};

use super::{rp, session};
use opensvn_svn_oracle as _;

/// Resolves the oracle or reports a skip. svn 1.14 and 1.15 write the same working copies for
/// everything these tests compare, so either runs them (`OPENSVN_SVN_BIN=<svn 1.14.x>`); a test
/// that needs 1.15 behaviour asks for [`oracle_1_15`].
pub fn oracle() -> Option<Oracle> {
    Oracle::resolve(MinVersion::V1_14).expect("oracle")
}

/// [`oracle`] for a case svn 1.14 gets wrong itself (it skips with a message on 1.14).
#[allow(dead_code)]
pub fn oracle_1_15() -> Option<Oracle> {
    Oracle::resolve(MinVersion::V1_15).expect("oracle")
}

/// A repository created by the real svn.
pub struct Repo {
    pub oracle: Oracle,
    pub dir: tempfile::TempDir,
    pub path: PathBuf,
    pub url: String,
}

fn run(mut cmd: Command, cwd: Option<&Path>) -> String {
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{cmd:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

impl Repo {
    pub fn new(oracle: &Oracle) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canon");
        let path = base.join("repo");
        let mut c = oracle.svnadmin();
        c.args(["create"]).arg(&path);
        run(c, None);
        let url = Oracle::file_url(&path);
        Self {
            oracle: oracle.clone(),
            dir,
            path,
            url,
        }
    }

    pub fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().expect("canon")
    }

    /// Runs `svn <args>` in `cwd`.
    pub fn svn(&self, cwd: &Path, args: &[&str]) -> String {
        let mut c = self.oracle.svn();
        c.args(args);
        run(c, Some(cwd))
    }

    pub fn checkout(&self, name: &str) -> PathBuf {
        let wc = self.base().join(name);
        let mut c = self.oracle.svn();
        c.args(["checkout", "-q", &self.url]).arg(&wc);
        run(c, None);
        wc
    }

    /// `svnadmin dump` replayed as fake revisions.
    pub fn dump_changes(&self) -> Vec<Vec<FakeChange>> {
        let mut c = self.oracle.svnadmin();
        c.args(["dump", "-q"]).arg(&self.path);
        let out = c.output().expect("dump");
        assert!(out.status.success(), "dump failed");
        parse_dump(&out.stdout)
    }

    /// A fake session holding the repository history up to `upto` revisions (all when `None`).
    pub fn fake(&self, upto: Option<usize>) -> FakeRaSession {
        let s = session();
        for (i, changes) in self.dump_changes().into_iter().enumerate() {
            if upto.is_some_and(|n| i >= n) {
                break;
            }
            s.seed("tester", "replay", &changes)
                .expect("replay revision");
        }
        s
    }
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn parse_props(body: &[u8]) -> Vec<(String, Option<Vec<u8>>)> {
    let mut out = Vec::new();
    let mut pos = 0;
    let line = |pos: &mut usize| -> Vec<u8> {
        let end = find(body, b"\n", *pos).expect("line");
        let l = body[*pos..end].to_vec();
        *pos = end + 1;
        l
    };
    loop {
        let l = line(&mut pos);
        if l == b"PROPS-END" {
            break;
        }
        let (kind, len) = {
            let s = String::from_utf8_lossy(&l).into_owned();
            let (k, n) = s.split_once(' ').expect("K/D n");
            (k.to_owned(), n.parse::<usize>().expect("len"))
        };
        let name = String::from_utf8_lossy(&body[pos..pos + len]).into_owned();
        pos += len + 1;
        if kind == "D" {
            out.push((name, None));
            continue;
        }
        let l = line(&mut pos);
        let vlen: usize = String::from_utf8_lossy(&l)
            .split_once(' ')
            .expect("V n")
            .1
            .parse()
            .expect("len");
        out.push((name, Some(body[pos..pos + vlen].to_vec())));
        pos += vlen + 1;
    }
    out
}

/// Parses `svnadmin dump` output (format 2, no deltas) into per-revision fake changes.
/// Copies are not supported; scenarios must not use `svn copy`.
pub fn parse_dump(data: &[u8]) -> Vec<Vec<FakeChange>> {
    let mut revs: Vec<Vec<FakeChange>> = Vec::new();
    let mut known_props: HashMap<String, BTreeMap<String, Vec<u8>>> = HashMap::new();
    let mut pos = 0;
    while pos < data.len() {
        // Header block.
        let end = match find(data, b"\n\n", pos) {
            Some(e) => e,
            None => break,
        };
        let header = String::from_utf8_lossy(&data[pos..end]).into_owned();
        pos = end + 2;
        if header.trim().is_empty() {
            continue;
        }
        let get = |k: &str| {
            header
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{k}: ")))
                .map(str::to_owned)
        };
        let clen: usize = get("Content-length").map_or(0, |v| v.parse().expect("clen"));
        let body = &data[pos..pos + clen];
        pos += clen;
        if get("Revision-number").is_some() {
            revs.push(Vec::new());
            if get("Revision-number").as_deref() == Some("0") {
                revs.pop();
            }
            continue;
        }
        let Some(path) = get("Node-path") else {
            continue;
        };
        let cur = revs.last_mut().expect("node before revision");
        let action = get("Node-action").expect("action");
        if action == "delete" {
            cur.push(FakeChange::Delete(rp(&path)));
            known_props.retain(|k, _| k != &path && !k.starts_with(&format!("{path}/")));
            continue;
        }
        if action == "replace" {
            // A delete and an add of the same path in one revision.
            cur.push(FakeChange::Delete(rp(&path)));
            known_props.retain(|k, _| k != &path && !k.starts_with(&format!("{path}/")));
        }
        assert!(
            get("Node-copyfrom-path").is_none(),
            "copies are not supported"
        );
        let kind = get("Node-kind").expect("kind");
        let plen: usize = get("Prop-content-length").map_or(0, |v| v.parse().expect("plen"));
        let tlen: Option<usize> = get("Text-content-length").map(|v| v.parse().expect("tlen"));
        let props_body = &body[..plen];
        let text = tlen.map(|t| Bytes::copy_from_slice(&body[plen..plen + t]));
        let mut new_props: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        if plen > 0 {
            for (k, v) in parse_props(props_body) {
                if let Some(v) = v {
                    new_props.insert(k, v);
                }
            }
        }
        if kind == "dir" {
            cur.push(FakeChange::PutDir(rp(&path)));
        } else if let Some(t) = text {
            cur.push(FakeChange::PutFile(rp(&path), t));
        }
        if plen > 0 || action == "add" || action == "replace" {
            let old = known_props.get(&path).cloned().unwrap_or_default();
            if plen > 0 {
                for (k, v) in &new_props {
                    if old.get(k) != Some(v) {
                        cur.push(FakeChange::SetProp(
                            rp(&path),
                            PropChange {
                                name: k.clone(),
                                value: Some(Bytes::copy_from_slice(v)),
                            },
                        ));
                    }
                }
                for k in old.keys().filter(|k| !new_props.contains_key(*k)) {
                    cur.push(FakeChange::SetProp(
                        rp(&path),
                        PropChange {
                            name: k.clone(),
                            value: None,
                        },
                    ));
                }
                known_props.insert(path.clone(), new_props);
            }
        }
    }
    revs
}

/// File, content and executable bit of every entry outside `.svn`.
pub fn tree_snapshot(root: &Path) -> BTreeMap<String, (bool, Vec<u8>, bool)> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, (bool, Vec<u8>, bool)>) {
        for e in std::fs::read_dir(dir).expect("read_dir") {
            let e = e.expect("entry");
            if e.file_name() == ".svn" {
                continue;
            }
            let path = e.path();
            let rel = path
                .strip_prefix(root)
                .expect("strip")
                .to_string_lossy()
                .replace('\\', "/");
            let md = std::fs::symlink_metadata(&path).expect("md");
            if md.file_type().is_symlink() {
                // The link itself, never what it points to (which may be missing or outside).
                let target = std::fs::read_link(&path).expect("read_link");
                let text = format!("link -> {}", target.to_string_lossy());
                out.insert(rel, (false, text.into_bytes(), false));
            } else if md.is_dir() {
                out.insert(rel, (true, Vec::new(), false));
                walk(root, &path, out);
            } else {
                #[cfg(unix)]
                let exec = {
                    use std::os::unix::fs::PermissionsExt;
                    md.permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let exec = false;
                out.insert(rel, (false, std::fs::read(&path).expect("read"), exec));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn props_text(blob: Option<Vec<u8>>) -> String {
    let Some(b) = blob.filter(|b| !b.is_empty()) else {
        return String::new();
    };
    let mut p = parse_proplist(&Skel::parse(&b).expect("skel")).expect("props");
    p.sort();
    p.iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                String::from_utf8_lossy(k),
                String::from_utf8_lossy(v)
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// NODES rows that do not depend on who wrote the working copy (no dates, authors or times).
pub fn nodes_snapshot(wc: &Path) -> Vec<String> {
    let db = WcDb::open(wc).expect("open");
    db.with_read(|c| {
        let mut stmt = c
            .prepare(
                "SELECT local_relpath, op_depth, presence, kind, revision, repos_path, checksum, \
                 depth, translated_size, properties, symlink_target FROM nodes \
                 ORDER BY local_relpath, op_depth",
            )
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| {
                Ok(format!(
                    "{}|{}|{}|{}|{:?}|{:?}|{:?}|{:?}|{:?}|{}",
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    props_text(r.get(9)?),
                ))
            })
            .expect("query");
        Ok(rows.map(|r| r.expect("row")).collect())
    })
    .expect("read")
}

/// ACTUAL_NODE rows (overlay properties, changelist, raw conflict data).
pub fn actual_snapshot(wc: &Path) -> Vec<String> {
    let db = WcDb::open(wc).expect("open");
    db.with_read(|c| {
        let mut stmt = c
            .prepare(
                "SELECT local_relpath, properties, changelist, conflict_data FROM actual_node \
                 ORDER BY local_relpath",
            )
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| {
                Ok(format!(
                    "{}|{}|{:?}|{}",
                    r.get::<_, String>(0)?,
                    props_text(r.get(1)?),
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<Vec<u8>>>(3)?
                        .map(|b| normalized_conflict(&b))
                        .unwrap_or_default(),
                ))
            })
            .expect("query");
        Ok(rows.map(|r| r.expect("row")).collect())
    })
    .expect("read")
}

/// Sorted `svn status -v` style listing run by the real svn on `wc`.
pub fn svn_status(oracle: &Oracle, wc: &Path, extra: &[&str]) -> String {
    let mut c = oracle.svn();
    c.args(["status"]).args(extra).current_dir(wc);
    let out = c.output().expect("status");
    assert!(
        out.status.success(),
        "svn status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines.join("\n")
}

/// Conflict skel with the repository identity replaced, so two different repositories compare.
fn normalized_conflict(blob: &[u8]) -> String {
    use svn_wc::conflicts::{ConflictSkel, Origin};
    let mut skel = ConflictSkel::from_skel(&Skel::parse(blob).expect("skel")).expect("conflict");
    if let Some(op) = skel.operation.as_mut() {
        for o in [&mut op.left, &mut op.right] {
            if let Origin::Subversion(l) = o {
                l.repos_root_url = "ROOT".into();
                l.repos_uuid = Some("UUID".into());
            }
        }
    }
    String::from_utf8_lossy(&skel.to_skel().unparse()).into_owned()
}

pub fn write(path: &Path, text: &str) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).expect("mkdir");
    }
    std::fs::write(path, text).expect("write");
}

pub fn assert_same(oracle: &Oracle, theirs: &Path, ours: &Path, what: &str) {
    assert_eq!(
        tree_snapshot(ours),
        tree_snapshot(theirs),
        "{what}: working files differ"
    );
    assert_eq!(
        nodes_snapshot(ours),
        nodes_snapshot(theirs),
        "{what}: NODES rows differ"
    );
    assert_eq!(
        actual_snapshot(ours),
        actual_snapshot(theirs),
        "{what}: ACTUAL rows differ"
    );
    assert_eq!(
        svn_status(oracle, ours, &[]),
        svn_status(oracle, theirs, &[]),
        "{what}: svn status differs"
    );
}

/// r1: files, properties, nested directories. Returns the seed working copy.
pub fn build_r1(repo: &Repo) -> std::path::PathBuf {
    let seed = repo.checkout("seed");
    write(&seed.join("README"), "hello\nworld\n");
    write(&seed.join("run.sh"), "#!/bin/sh\necho hi\n");
    write(
        &seed.join("docs/guide.txt"),
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\n",
    );
    write(&seed.join("src/lib/deep.rs"), "fn deep() {}\n");
    write(&seed.join("empty.txt"), "");
    repo.svn(
        &seed,
        &["add", "-q", "README", "run.sh", "docs", "src", "empty.txt"],
    );
    repo.svn(&seed, &["propset", "-q", "svn:executable", "*", "run.sh"]);
    repo.svn(
        &seed,
        &["propset", "-q", "svn:eol-style", "native", "docs/guide.txt"],
    );
    repo.svn(&seed, &["propset", "-q", "color", "red", "docs"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    seed
}

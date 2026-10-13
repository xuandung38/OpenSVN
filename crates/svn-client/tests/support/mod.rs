//! Shared helpers: real svn working copies through the oracle, wc.db row dumps and handles.
#![allow(dead_code, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use opensvn_svn_oracle::{MinVersion, Oracle};
use svn_types::{MemoryLockOwnership, RelPath};
use svn_wc::WcDb;
use svn_wc::db::{LockOwner, find_wcroot};

/// svn 1.15 or `None` (test skips; `OPENSVN_REQUIRE_SVN=1` makes absence fatal).
pub fn oracle() -> Option<Oracle> {
    Oracle::resolve(MinVersion::V1_15).expect("oracle")
}

/// Runs svn in `cwd` and panics with stderr on failure.
pub fn svn(o: &Oracle, cwd: &Path, args: &[&str]) {
    let out = o
        .svn()
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run svn");
    assert!(
        out.status.success(),
        "svn {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Creates a repository under `base/repo` and returns its file URL.
pub fn create_repo(o: &Oracle, base: &Path) -> String {
    let st = o
        .svnadmin()
        .args(["create", "repo"])
        .current_dir(base)
        .status()
        .expect("svnadmin");
    assert!(st.success());
    Oracle::file_url(&base.join("repo"))
}

/// `svn checkout` into `base/name`.
pub fn checkout(o: &Oracle, base: &Path, url: &str, name: &str) -> PathBuf {
    svn(o, base, &["checkout", "-q", url, name]);
    base.join(name)
}

pub fn write(wc: &Path, rel: &str, text: &str) {
    let p = wc.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(p, text).expect("write");
}

pub fn read(wc: &Path, rel: &str) -> String {
    fs::read_to_string(wc.join(rel)).expect("read")
}

pub fn rp(s: &str) -> RelPath {
    RelPath::new(s).expect("relpath")
}

/// Opens the working copy with a lock owner registered.
pub fn open(wc: &Path) -> WcDb {
    let root = find_wcroot(wc).expect("root");
    let db = WcDb::open(&root).expect("open");
    db.set_lock_owner(LockOwner {
        store: Arc::new(MemoryLockOwnership::for_current_process("test-boot")),
        pid: std::process::id(),
        boot_id: "test-boot".to_owned(),
    });
    db
}

/// Repository with `d/{f1(prop k=v),f2,sub/g}`, `e/` and `top.txt` at r1.
pub struct Fixture {
    pub oracle: Oracle,
    pub tmp: tempfile::TempDir,
    pub url: String,
    pub base: PathBuf,
}

pub fn fixture() -> Option<Fixture> {
    let oracle = oracle()?;
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canon");
    let url = create_repo(&oracle, &base);
    let seed = checkout(&oracle, &base, &url, "seed");
    write(&seed, "d/f1", "1\n");
    write(&seed, "d/f2", "2\n");
    write(&seed, "d/sub/g", "3\n");
    write(&seed, "top.txt", "t\n");
    fs::create_dir(seed.join("e")).expect("mkdir");
    svn(&oracle, &seed, &["add", "-q", "d", "e", "top.txt"]);
    svn(&oracle, &seed, &["propset", "-q", "k", "v", "d/f1"]);
    svn(&oracle, &seed, &["commit", "-q", "-m", "seed"]);
    Some(Fixture {
        oracle,
        tmp,
        url,
        base,
    })
}

fn norm_props(blob: Option<Vec<u8>>) -> String {
    let Some(blob) = blob else {
        return "NULL".into();
    };
    let skel = svn_wc::skel::Skel::parse(&blob).expect("skel");
    let mut props = svn_wc::skel::parse_proplist(&skel).unwrap_or_default();
    props.sort();
    props
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                String::from_utf8_lossy(k),
                String::from_utf8_lossy(v)
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Every NODES, ACTUAL_NODE and PRISTINE row as sorted text lines.
pub fn dump_rows(wc: &Path) -> Vec<String> {
    let db = WcDb::open(wc).expect("open wc.db");
    db.with_read(|conn| {
    let mut out = Vec::new();
    let mut stmt = conn
        .prepare(
            "SELECT local_relpath, op_depth, parent_relpath, repos_id, repos_path, revision, presence, \
             moved_here, moved_to, kind, properties, depth, checksum, symlink_target, changed_revision, \
             changed_date, changed_author, file_external, inherited_props \
             FROM nodes ORDER BY local_relpath, op_depth",
        )
        .expect("nodes");
    let rows = stmt
        .query_map([], |r| {
            Ok(format!(
                "N {:?} od={} parent={:?} repos={:?}:{:?}@{:?} {:?} mh={:?} mt={:?} {:?} props={} depth={:?} ck={:?} sl={:?} cr={:?} cd={:?} ca={:?} fe={:?} ip={}",
                r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<i64>>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, Option<i64>>(5)?,
                r.get::<_, String>(6)?, r.get::<_, Option<i64>>(7)?, r.get::<_, Option<String>>(8)?,
                r.get::<_, String>(9)?, norm_props(r.get(10)?), r.get::<_, Option<String>>(11)?,
                r.get::<_, Option<String>>(12)?, r.get::<_, Option<String>>(13)?, r.get::<_, Option<i64>>(14)?,
                r.get::<_, Option<i64>>(15)?, r.get::<_, Option<String>>(16)?,
                r.get::<_, Option<i64>>(17)?, norm_props(r.get(18)?)
            ))
        })
        .expect("q");
    out.extend(rows.map(|r| r.expect("row")));
    let mut stmt = conn
        .prepare("SELECT local_relpath, parent_relpath, properties, changelist, conflict_data IS NOT NULL, text_mod FROM actual_node ORDER BY local_relpath")
        .expect("actual");
    let rows = stmt
        .query_map([], |r| {
            Ok(format!(
                "A {:?} parent={:?} props={} cl={:?} conflict={} tm={:?}",
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                norm_props(r.get(2)?),
                r.get::<_, Option<String>>(3)?,
                r.get::<_, bool>(4)?,
                r.get::<_, Option<String>>(5)?
            ))
        })
        .expect("q");
    out.extend(rows.map(|r| r.expect("row")));
    let mut stmt = conn
        .prepare("SELECT checksum, size, refcount, md5_checksum FROM pristine ORDER BY checksum")
        .expect("pristine");
    let rows = stmt
        .query_map([], |r| {
            Ok(format!(
                "P {} size={} ref={} {}",
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?
            ))
        })
        .expect("q");
    out.extend(rows.map(|r| r.expect("row")));
    Ok(out)
    })
    .expect("read wc.db")
}

/// `svn status --xml -v` of `wc` with path and attribute order normalised.
pub fn status_xml(o: &Oracle, wc: &Path) -> String {
    let out = o
        .svn()
        .args(["status", "--xml", "-v"])
        .current_dir(wc)
        .output()
        .expect("svn status");
    assert!(
        out.status.success(),
        "svn status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let root = wc.canonicalize().expect("canon").display().to_string();
    let flat = text.replace(&root, "WC").replace('\n', " ");
    let mut out = String::new();
    for (i, piece) in flat.split('<').enumerate() {
        if i == 0 {
            continue;
        }
        let (tag, rest) = piece.split_once('>').unwrap_or((piece, ""));
        let mut parts = tag.split_whitespace();
        let name = parts.next().unwrap_or("");
        let mut attrs: Vec<&str> = parts.collect();
        attrs.sort_unstable();
        out.push_str(&format!("<{name} {}>{}\n", attrs.join(" "), rest.trim()));
    }
    out.lines()
        .filter(|l| !l.starts_with("<date"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Asserts two row dumps are equal with a readable diff.
pub fn assert_rows_equal(label: &str, svn_rows: &[String], our_rows: &[String]) {
    if svn_rows == our_rows {
        return;
    }
    let mut msg = format!("{label}: NODES/ACTUAL/PRISTINE rows differ\n");
    for l in svn_rows.iter().filter(|l| !our_rows.contains(l)) {
        msg.push_str(&format!("  only svn:  {l}\n"));
    }
    for l in our_rows.iter().filter(|l| !svn_rows.contains(l)) {
        msg.push_str(&format!("  only ours: {l}\n"));
    }
    panic!("{msg}");
}

pub mod rec_ra;

/// `svn status` (no server contact) text of `wc`.
pub fn svn_status(o: &Oracle, wc: &Path) -> String {
    svn_out(o, wc, &["status"])
}

/// Runs svn and returns stdout, panicking on failure.
pub fn svn_out(o: &Oracle, cwd: &Path, args: &[&str]) -> String {
    let out = o
        .svn()
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run svn");
    assert!(
        out.status.success(),
        "svn {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

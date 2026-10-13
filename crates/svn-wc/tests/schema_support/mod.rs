//! Shared helpers for the wc.db schema/open tests: builds real working copies with the local
//! `svn`/`svnadmin` binaries and dumps a structural description of a wc.db.
#![allow(dead_code, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::{Connection, OpenFlags};

/// Returns the `svn --version --quiet` output, or `None` when svn is not installed.
pub fn svn_version() -> Option<String> {
    let out = Command::new("svn")
        .args(["--version", "--quiet"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// True when the installed svn is 1.15 or newer (needed for `--compatible-version=1.15`).
pub fn svn_is_1_15_or_newer() -> bool {
    svn_version().is_some_and(|v| {
        let mut parts = v.split('.').filter_map(|p| p.parse::<u32>().ok());
        let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        (major, minor) >= (1, 15)
    })
}

/// Skips (returns `true`) when svn is older than 1.15, with a visible note.
pub fn skip_without_svn_1_15() -> bool {
    if skip_without_svn() {
        return true;
    }
    if !svn_is_1_15_or_newer() {
        eprintln!("svn older than 1.15: skipping format 32 test");
        return true;
    }
    false
}

/// Skips (returns `true`) when svn/svnadmin are unavailable so the suite still passes on
/// machines without Subversion; CI images that carry svn run the real comparison.
pub fn skip_without_svn() -> bool {
    if svn_version().is_none() {
        eprintln!("svn not found on PATH: skipping svn-backed test");
        return true;
    }
    false
}

fn run(cmd: &str, args: &[&str], cwd: &Path) {
    let out = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("cannot run {cmd}: {e}"));
    assert!(
        out.status.success(),
        "{cmd} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Creates a repository under `base/repo` and returns its `file://` URL.
pub fn create_repo(base: &Path) -> String {
    run("svnadmin", &["create", "repo"], base);
    let abs = base.canonicalize().expect("canonicalize").join("repo");
    format!("file://{}", abs.display())
}

/// Runs `svn <args>` in `cwd`, panicking with stderr on failure.
pub fn svn(cwd: &Path, args: &[&str]) {
    run("svn", args, cwd);
}

/// Checks out `url` into `base/<name>`; extra args (e.g. `--compatible-version=1.15`) are
/// inserted before the URL.
pub fn checkout(base: &Path, url: &str, name: &str, extra: &[&str]) -> PathBuf {
    let mut args = vec!["checkout", "-q"];
    args.extend_from_slice(extra);
    args.push(url);
    args.push(name);
    run("svn", &args, base);
    base.join(name)
}

/// Opens a wc.db read-only for inspection.
pub fn open_ro(wc: &Path) -> Connection {
    Connection::open_with_flags(
        wc.join(".svn/wc.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("open wc.db")
}

/// Removes `/* ... */` comments (svn's build strips them from the SQL it embeds) and collapses
/// whitespace.
fn collapse_ws(sql: &str) -> String {
    let mut plain = String::new();
    let mut rest = sql;
    while let Some(start) = rest.find("/*") {
        plain.push_str(&rest[..start]);
        rest = rest[start..]
            .find("*/")
            .map_or("", |end| &rest[start + end + 2..]);
    }
    plain.push_str(rest);
    plain
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Structural description of a wc.db: one entry per schema object, values are canonical strings.
/// Table SQL text is *not* compared; columns, keys, indexes and statistics are. Trigger and view
/// bodies cannot be introspected, so their whitespace-normalised, lower-cased text is compared.
pub fn structure(conn: &Connection) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let pragma = |name: &str| -> i64 {
        conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
            .expect("pragma")
    };
    out.insert(
        "pragma:user_version".into(),
        pragma("user_version").to_string(),
    );
    out.insert(
        "pragma:application_id".into(),
        pragma("application_id").to_string(),
    );
    out.insert("pragma:page_size".into(), pragma("page_size").to_string());
    out.insert(
        "pragma:auto_vacuum".into(),
        pragma("auto_vacuum").to_string(),
    );

    let mut stmt = conn
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name")
        .expect("prepare");
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows");
    for (kind, name, tbl, sql) in rows {
        // Optional planner table created by ANALYZE only when SQLite is built with STAT4; its
        // presence depends on the SQLite build, not on the svn schema.
        if name == "sqlite_stat4" {
            continue;
        }
        match kind.as_str() {
            "table" => {
                let mut cols = Vec::new();
                let mut s = conn
                    .prepare(&format!("PRAGMA table_info('{name}')"))
                    .expect("ti");
                let it = s
                    .query_map([], |r| {
                        Ok(format!(
                            "{}:{}:nn={}:dflt={:?}:pk={}",
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?.to_uppercase(),
                            r.get::<_, i64>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, i64>(5)?
                        ))
                    })
                    .expect("cols");
                for c in it {
                    cols.push(c.expect("col"));
                }
                let mut fks = Vec::new();
                let mut s = conn
                    .prepare(&format!("PRAGMA foreign_key_list('{name}')"))
                    .expect("fk");
                let it = s
                    .query_map([], |r| {
                        Ok(format!(
                            "{}->{}.{:?}",
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(4)?
                        ))
                    })
                    .expect("fks");
                for f in it {
                    fks.push(f.expect("fk"));
                }
                let autoinc = sql
                    .as_deref()
                    .is_some_and(|s| s.to_lowercase().contains("autoincrement"));
                out.insert(
                    format!("table:{name}"),
                    format!(
                        "cols=[{}] fks=[{}] autoinc={autoinc}",
                        cols.join(", "),
                        fks.join(", ")
                    ),
                );
            }
            "index" => {
                let mut s = conn
                    .prepare(&format!("PRAGMA index_info('{name}')"))
                    .expect("ii");
                let it = s
                    .query_map([], |r| r.get::<_, Option<String>>(2))
                    .expect("cols");
                let cols: Vec<String> = it.map(|c| format!("{:?}", c.expect("c"))).collect();
                let mut s = conn
                    .prepare(&format!("PRAGMA index_list('{tbl}')"))
                    .expect("il");
                let meta = s
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, i64>(4)?,
                        ))
                    })
                    .expect("il")
                    .map(|m| m.expect("m"))
                    .find(|(n, ..)| *n == name)
                    .map(|(_, u, o, p)| format!("unique={u} origin={o} partial={p}"))
                    .unwrap_or_default();
                out.insert(
                    format!("index:{name}"),
                    format!("on={tbl} cols=[{}] {meta}", cols.join(",")),
                );
            }
            "trigger" | "view" => {
                out.insert(
                    format!("{kind}:{name}"),
                    format!("on={tbl} {}", collapse_ws(sql.as_deref().unwrap_or(""))),
                );
            }
            other => {
                out.insert(format!("{other}:{name}"), String::new());
            }
        }
    }
    if out.contains_key("table:sqlite_stat1") {
        let mut s = conn
            .prepare("SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl, idx")
            .expect("stat");
        let it = s
            .query_map([], |r| {
                Ok(format!(
                    "{}|{:?}|{}",
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?
                ))
            })
            .expect("stat rows");
        let stats: Vec<String> = it.map(|r| r.expect("row")).collect();
        out.insert("stat1".into(), stats.join("; "));
    }
    out
}

/// Asserts two structures are equal, printing every differing key.
pub fn assert_same_structure(
    left_name: &str,
    left: &BTreeMap<String, String>,
    right_name: &str,
    right: &BTreeMap<String, String>,
) {
    let mut diffs = Vec::new();
    for key in left
        .keys()
        .chain(right.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        if left.get(key) != right.get(key) {
            diffs.push(format!(
                "{key}\n  {left_name}: {:?}\n  {right_name}: {:?}",
                left.get(key),
                right.get(key)
            ));
        }
    }
    assert!(
        diffs.is_empty(),
        "schema structure differs:\n{}",
        diffs.join("\n")
    );
}

/// Parses the stored property skel into a sorted, printable list so hash-ordered svn output and
/// our sorted output compare equal.
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

/// Every NODES, ACTUAL_NODE and PRISTINE row as sorted text lines (dav_cache excluded).
pub fn dump_rows(wc: &Path) -> Vec<String> {
    let conn = open_ro(wc);
    let mut out = Vec::new();
    let mut stmt = conn
        .prepare(
            "SELECT local_relpath, op_depth, parent_relpath, repos_id, repos_path, revision, presence, \
             moved_here, moved_to, kind, properties, depth, checksum, symlink_target, changed_revision, \
             changed_date, changed_author, 0, file_external, inherited_props \
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
                r.get::<_, Option<i64>>(18)?, norm_props(r.get(19)?)
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
    out
}

/// `svn status --xml -v` with the working copy path masked, for cross-checking two copies.
pub fn status_xml(wc: &Path) -> String {
    let out = Command::new("svn")
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
    // svn prints XML attributes in hash order: sort them inside every tag.
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

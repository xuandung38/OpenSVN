//! `svn:externals` against svn 1.15: the same history and the same operations, run once by the
//! real svn and once by this client (two repositories replayed into fake sessions), must leave
//! identical working copies: files, NODES rows of every working copy, `EXTERNALS` rows, ACTUAL
//! rows and `svn status` output. Skipped when no svn 1.14 or newer is installed (fails instead when
//! `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::oracle::*;
use common::*;
use opensvn_svn_oracle::Oracle;
use svn_client::{ExternalAction, StatusOptions, checkout, update};
use svn_ra_api::fake::FakeRaSession;
use svn_types::{CancelToken, Depth, NodeStatus, RelPath, RepoUrl};
use svn_wc::db::WcDb;

const MAIN: &str = "https://svn.test.local/repos/repo";
const OTHER: &str = "https://svn.test.local/repos/repo2";

/// Two sibling repositories created by the real svn (`repo` and `repo2`), so `^/../repo2`
/// resolves the same way for both clients.
struct Repos {
    oracle: Oracle,
    _dir: tempfile::TempDir,
    base: PathBuf,
    main: PathBuf,
    other: PathBuf,
}

impl Repos {
    fn new(oracle: &Oracle) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canon");
        let (main, other) = (base.join("repo"), base.join("repo2"));
        for p in [&main, &other] {
            let out = oracle
                .svnadmin()
                .arg("create")
                .arg(p)
                .output()
                .expect("svnadmin");
            assert!(out.status.success());
        }
        Self {
            oracle: oracle.clone(),
            _dir: dir,
            base,
            main,
            other,
        }
    }

    fn url(&self, p: &Path, below: &str) -> String {
        let u = Oracle::file_url(p);
        if below.is_empty() {
            u
        } else {
            format!("{u}/{below}")
        }
    }

    fn svn(&self, cwd: &Path, args: &[&str]) -> String {
        let out = self
            .oracle
            .svn()
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("svn");
        assert!(
            out.status.success() || args.first() == Some(&"update"),
            "svn {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn fake_of(&self, path: &Path, url: &str, uuid: &str) -> FakeRaSession {
        let out = self
            .oracle
            .svnadmin()
            .args(["dump", "-q"])
            .arg(path)
            .output()
            .expect("dump");
        assert!(out.status.success());
        let s = FakeRaSession::new(url, uuid).expect("fake");
        for changes in parse_dump(&out.stdout) {
            s.seed("tester", "replay", &changes).expect("replay");
        }
        s
    }

    /// Fresh fakes holding the current history of both repositories.
    fn fakes(&self) -> (FakeRaSession, Arc<FakeOpener>) {
        let main = self.fake_of(&self.main, MAIN, "uuid-repo");
        let other = self.fake_of(&self.other, OTHER, "uuid-repo2");
        let opener = FakeOpener::new(vec![main.clone(), other]);
        (main, opener)
    }

    fn trunk(main: &FakeRaSession) -> FakeRaSession {
        main.at(&RepoUrl::parse(&format!("{MAIN}/trunk")).unwrap())
            .unwrap()
    }
}

fn props_file(dir: &Path, text: &str) -> PathBuf {
    let p = dir.join(format!("defs-{}.txt", text.len()));
    std::fs::write(&p, text).unwrap();
    p
}

/// `svn propset svn:externals -F` (values starting with `-r` cannot be given inline).
fn set_externals(r: &Repos, wc: &Path, target: &str, text: &str) {
    let f = props_file(&r.base, text);
    r.svn(
        wc,
        &[
            "propset",
            "-q",
            "svn:externals",
            "-F",
            f.to_str().unwrap(),
            target,
        ],
    );
}

/// r1: `lib` (two files and a subdirectory), `trunk/sub`; repo2: one file.
/// r2: externals on `trunk` of every kind the URLs of both clients can share, and a nested
/// file external on `lib/sub`.
fn build(r: &Repos) -> PathBuf {
    let seed = r.base.join("seed");
    r.svn(&r.base, &["checkout", "-q", &r.url(&r.main, ""), "seed"]);
    write(&seed.join("lib/a.txt"), "alpha\n");
    write(&seed.join("lib/f.txt"), "1\n2\n3\n4\n5\n");
    write(&seed.join("lib/sub/s.txt"), "sub\n");
    write(&seed.join("trunk/sub/keep.txt"), "keep\n");
    r.svn(&seed, &["add", "-q", "lib", "trunk"]);
    r.svn(&seed, &["commit", "-q", "-m", "r1"]);
    let seed2 = r.base.join("seed2");
    r.svn(&r.base, &["checkout", "-q", &r.url(&r.other, ""), "seed2"]);
    write(&seed2.join("r2.txt"), "other repository\n");
    r.svn(&seed2, &["add", "-q", "r2.txt"]);
    r.svn(&seed2, &["commit", "-q", "-m", "r1"]);

    set_externals(
        r,
        &seed,
        "trunk",
        "^/lib ext\n-r 1 ^/lib pinned\n^/lib@1 pegged\n^/lib/f.txt fext\n\
         ^/lib/f.txt@1 sub/pfext\n^/../repo2 other\n../lib/sub up\n# comment\n",
    );
    set_externals(r, &seed, "lib/sub", "^/lib/a.txt nest\n");
    r.svn(&seed, &["commit", "-q", "-m", "r2"]);
    seed
}

/// `EXTERNALS` rows with the repository named by the last segment of its root URL.
fn externals_snapshot(wc: &Path) -> Vec<String> {
    let db = WcDb::open(wc).expect("open");
    db.with_read(|c| {
        let mut stmt = c
            .prepare(
                "SELECT e.local_relpath, e.parent_relpath, e.presence, e.kind, \
                 e.def_local_relpath, e.def_repos_relpath, e.def_operational_revision, \
                 e.def_revision, r.root FROM externals e JOIN repository r ON r.id = e.repos_id \
                 ORDER BY e.local_relpath",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                let root: String = r.get(8)?;
                Ok(format!(
                    "{}|{}|{}|{}|{}|{}|{:?}|{:?}|{}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    root.rsplit('/').next().unwrap_or_default(),
                ))
            })
            .unwrap();
        Ok(rows.map(|r| r.unwrap()).collect())
    })
    .unwrap()
}

/// BASE columns that mark file externals (whether the inherited-properties cache is set, only for
/// them: the root's cache is outside this test's scope).
fn file_external_snapshot(wc: &Path) -> Vec<String> {
    let db = WcDb::open(wc).expect("open");
    db.with_read(|c| {
        let mut stmt = c
            .prepare(
                "SELECT local_relpath, file_external, inherited_props, \
                 translated_size IS NOT NULL, last_mod_time IS NOT NULL FROM nodes \
                 WHERE op_depth = 0 ORDER BY local_relpath",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                let fe: Option<i64> = r.get(1)?;
                Ok(format!(
                    "{}|{:?}|{:?}|{}|{}",
                    r.get::<_, String>(0)?,
                    fe,
                    // svn caches the properties the repository ancestors pass down; we record
                    // an empty cache (see `svn_client::externals`), so only presence compares.
                    r.get::<_, Option<Vec<u8>>>(2)?
                        .filter(|_| fe.is_some())
                        .map(|_| "cached"),
                    r.get::<_, bool>(3)?,
                    r.get::<_, bool>(4)?,
                ))
            })
            .unwrap();
        Ok(rows.map(|r| r.unwrap()).collect())
    })
    .unwrap()
}

/// Working copy roots below `root` (externals), relative, sorted.
fn nested_wcs(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            if e.file_name() == ".svn" || !p.symlink_metadata().unwrap().is_dir() {
                continue;
            }
            if p.join(".svn/wc.db").is_file() {
                out.push(
                    p.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
            walk(root, &p, out);
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn assert_same_externals(oracle: &Oracle, theirs: &Path, ours: &Path, what: &str) {
    assert_same_except(oracle, theirs, ours, what, &[]);
}

/// [`assert_same_externals`] with the working files at `skip` left out of the comparison.
fn assert_same_except(oracle: &Oracle, theirs: &Path, ours: &Path, what: &str, skip: &[&str]) {
    let tree = |p: &Path| {
        let mut t = tree_snapshot(p);
        t.retain(|k, _| !skip.contains(&k.as_str()));
        t
    };
    assert_eq!(tree(ours), tree(theirs), "{what}: working files differ");
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
    let wcs = nested_wcs(theirs);
    assert_eq!(
        nested_wcs(ours),
        wcs,
        "{what}: external working copies differ"
    );
    for sub in std::iter::once(String::new()).chain(wcs) {
        let (t, o) = (theirs.join(&sub), ours.join(&sub));
        assert_eq!(
            externals_snapshot(&o),
            externals_snapshot(&t),
            "{what}: EXTERNALS rows of '{sub}' differ"
        );
        assert_eq!(
            file_external_snapshot(&o),
            file_external_snapshot(&t),
            "{what}: file external columns of '{sub}' differ"
        );
        if !sub.is_empty() {
            assert_eq!(
                nodes_snapshot(&o),
                nodes_snapshot(&t),
                "{what}: NODES rows of '{sub}' differ"
            );
            assert_eq!(
                actual_snapshot(&o),
                actual_snapshot(&t),
                "{what}: ACTUAL rows of '{sub}' differ"
            );
        }
    }
}

async fn our_checkout(r: &Repos, sb: &Sandbox, name: &str) -> (PathBuf, svn_client::UpdateSummary) {
    let (main, opener) = r.fakes();
    let ours = sb.path(name);
    let sum = checkout(
        &ext_ctx(opener),
        &Repos::trunk(&main),
        &ours,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    (ours, sum)
}

async fn our_update(r: &Repos, wc: &Path) -> svn_client::UpdateSummary {
    let (main, opener) = r.fakes();
    update(
        &ext_ctx(opener),
        &Repos::trunk(&main),
        wc,
        None,
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect("update")
}

#[tokio::test(flavor = "multi_thread")]
async fn checkout_fetches_every_kind_of_external_like_svn() {
    let Some(oracle) = oracle() else { return };
    let r = Repos::new(&oracle);
    build(&r);
    let theirs = r.base.join("theirs");
    r.svn(
        &r.base,
        &["checkout", "-q", &r.url(&r.main, "trunk"), "theirs"],
    );
    let sb = Sandbox::new();
    let (ours, sum) = our_checkout(&r, &sb, "ours").await;
    assert_same_externals(&oracle, &theirs, &ours, "checkout");

    let fetched: Vec<_> = sum
        .externals
        .iter()
        .map(|e| (e.path.as_str().to_owned(), e.action))
        .collect();
    for p in [
        "ext",
        "pinned",
        "pegged",
        "fext",
        "sub/pfext",
        "other",
        "up",
        "up/nest",
        "ext/sub/nest",
    ] {
        assert!(
            fetched.contains(&(p.to_owned(), ExternalAction::Fetched)),
            "{p} not fetched: {fetched:?}"
        );
    }
    assert_eq!(sum.external_warnings().count(), 0, "{:?}", sum.externals);
    assert!(
        sum.events.iter().any(|e| e.path.as_str() == "ext/a.txt"),
        "events of externals carry full paths"
    );

    // Our own status: directory externals are `X`, file externals are flagged.
    let st = svn_client::status(
        &ctx(),
        &ours,
        &StatusOptions::default(),
        &CancelToken::new(),
    )
    .await
    .expect("status");
    let x: Vec<_> = st
        .iter()
        .filter(|s| s.text == NodeStatus::External)
        .map(|s| s.path.as_str().to_owned())
        .collect();
    assert_eq!(x, ["ext", "other", "pegged", "pinned", "up"]);
    let all = svn_client::status(
        &ctx(),
        &ours,
        &StatusOptions {
            show_unmodified: true,
            ..StatusOptions::default()
        },
        &CancelToken::new(),
    )
    .await
    .expect("status -v");
    let fx: Vec<_> = all
        .iter()
        .filter(|s| s.file_external)
        .map(|s| s.path.as_str().to_owned())
        .collect();
    assert_eq!(fx, ["fext", "sub/pfext"]);

    // svn itself works on the working copy we wrote, externals included.
    r.svn(&ours, &["cleanup"]);
    r.svn(&ours, &["info", "ext", "fext"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn update_adds_switches_and_removes_externals_like_svn() {
    let Some(oracle) = oracle() else { return };
    let r = Repos::new(&oracle);
    let seed = build(&r);
    let theirs = r.base.join("theirs");
    r.svn(
        &r.base,
        &["checkout", "-q", &r.url(&r.main, "trunk"), "theirs"],
    );
    let sb = Sandbox::new();
    let (ours, _) = our_checkout(&r, &sb, "ours").await;

    // Local changes: an edit in a directory external that goes away (kept), an unversioned
    // file in another one (only that file stays).
    for wc in [&theirs, &ours] {
        write(&wc.join("ext/a.txt"), "alpha\nlocal\n");
        write(&wc.join("other/unversioned.txt"), "mine\n");
    }
    // r3: incoming change of the file external's source, new definitions.
    write(&seed.join("lib/f.txt"), "1\n2\n3\n4\nfive\n");
    set_externals(
        &r,
        &seed,
        "trunk",
        "-r 2 ^/lib pinned\n^/lib/sub pegged\n^/lib/f.txt fext\n^/lib/sub/s.txt sub/fnew\n\
         ^/lib/sub added/deeper/ext\n",
    );
    r.svn(&seed, &["commit", "-q", "-m", "r3"]);

    r.svn(&theirs, &["update", "-q"]);
    let sum = our_update(&r, &ours).await;
    assert_same_externals(&oracle, &theirs, &ours, "update");

    let by_path = |p: &str| {
        sum.externals
            .iter()
            .find(|e| e.path.as_str() == p)
            .unwrap_or_else(|| panic!("no report for {p}: {:?}", sum.externals))
            .clone()
    };
    assert_eq!(
        by_path("ext").action,
        ExternalAction::LeftLocalModifications
    );
    assert!(
        by_path("ext")
            .message
            .unwrap()
            .contains("local modifications")
    );
    assert_eq!(
        by_path("other").action,
        ExternalAction::LeftLocalModifications
    );
    assert_eq!(by_path("sub/pfext").action, ExternalAction::Removed);
    assert_eq!(
        by_path("up").action,
        ExternalAction::Removed,
        "unmodified, with a nested file external"
    );
    assert!(!ours.join("up").exists());
    assert_eq!(by_path("pegged").action, ExternalAction::Updated);
    assert_eq!(by_path("sub/fnew").action, ExternalAction::Fetched);
    assert_eq!(by_path("added/deeper/ext").action, ExternalAction::Fetched);
    assert_eq!(read(&ours.join("fext")), "1\n2\n3\n4\nfive\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_change_in_a_file_external_is_merged_where_svn_conflicts() {
    // Deliberate difference from svn 1.15: svn merges a file external against an empty base,
    // so every local change conflicts (and `.rOLD` is empty). We merge against the old text,
    // like for any other file, so a change elsewhere in the file merges cleanly.
    let Some(oracle) = oracle() else { return };
    let r = Repos::new(&oracle);
    let seed = build(&r);
    let theirs = r.base.join("theirs");
    r.svn(
        &r.base,
        &["checkout", "-q", &r.url(&r.main, "trunk"), "theirs"],
    );
    let sb = Sandbox::new();
    let (ours, _) = our_checkout(&r, &sb, "ours").await;
    for wc in [&theirs, &ours] {
        write(&wc.join("fext"), "one\n2\n3\n4\n5\n");
    }
    write(&seed.join("lib/f.txt"), "1\n2\n3\n4\nfive\n");
    r.svn(&seed, &["commit", "-q", "-m", "r3"]);
    let _ = r
        .oracle
        .svn()
        .args(["update", "-q", "--accept", "postpone"])
        .current_dir(&theirs)
        .output();
    let sum = our_update(&r, &ours).await;
    assert!(read(&theirs.join("fext")).starts_with("<<<<<<< .mine"));
    assert_eq!(read(&ours.join("fext")), "one\n2\n3\n4\nfive\n");
    assert!(
        sum.events
            .iter()
            .any(|e| e.path.as_str() == "fext" && e.action == svn_client::UpdateAction::Merged)
    );
    assert_eq!(r.svn(&ours, &["status", "fext"]).trim(), "M   X   fext");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_modified_file_external_is_kept_when_its_definition_goes() {
    // Deliberate difference from svn 1.15, which deletes the modified file: we keep it as an
    // unversioned file. Everything else matches.
    let Some(oracle) = oracle() else { return };
    let r = Repos::new(&oracle);
    let seed = build(&r);
    let theirs = r.base.join("theirs");
    r.svn(
        &r.base,
        &["checkout", "-q", &r.url(&r.main, "trunk"), "theirs"],
    );
    let sb = Sandbox::new();
    let (ours, _) = our_checkout(&r, &sb, "ours").await;
    for wc in [&theirs, &ours] {
        write(&wc.join("fext"), "mine\n");
    }
    set_externals(&r, &seed, "trunk", "^/lib ext\n");
    r.svn(&seed, &["commit", "-q", "-m", "r3"]);
    r.svn(&theirs, &["update", "-q"]);
    let sum = our_update(&r, &ours).await;

    assert!(!theirs.join("fext").exists(), "svn deletes it");
    assert_eq!(read(&ours.join("fext")), "mine\n", "we keep it");
    let fext = sum
        .externals
        .iter()
        .find(|e| e.path.as_str() == "fext")
        .unwrap();
    assert_eq!(fext.action, ExternalAction::LeftLocalModifications);
    std::fs::remove_file(ours.join("fext")).unwrap();
    assert_same_externals(&oracle, &theirs, &ours, "update");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conflicting_change_in_a_file_external_conflicts_like_svn() {
    let Some(oracle) = oracle() else { return };
    let r = Repos::new(&oracle);
    let seed = build(&r);
    let theirs = r.base.join("theirs");
    r.svn(
        &r.base,
        &["checkout", "-q", &r.url(&r.main, "trunk"), "theirs"],
    );
    let sb = Sandbox::new();
    let (ours, _) = our_checkout(&r, &sb, "ours").await;
    for wc in [&theirs, &ours] {
        write(&wc.join("fext"), "1\n2\nmine\n4\n5\n");
    }
    write(&seed.join("lib/f.txt"), "1\n2\ntheirs\n4\n5\n");
    r.svn(&seed, &["commit", "-q", "-m", "r3"]);
    let _ = r
        .oracle
        .svn()
        .args(["update", "-q", "--accept", "postpone"])
        .current_dir(&theirs)
        .output();
    our_update(&r, &ours).await;
    // The conflict is the same (rows, markers, status); only the merged text and the `.rOLD`
    // file differ, because svn merges file externals against an empty base (see above).
    assert_same_except(
        &oracle,
        &theirs,
        &ours,
        "conflicting update",
        &["fext", "fext.r2"],
    );
    assert_eq!(read(&ours.join("fext.r2")), "1\n2\n3\n4\n5\n");
    assert_eq!(read(&ours.join("fext.mine")), "1\n2\nmine\n4\n5\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn local_definitions_are_fetched_by_update_like_svn() {
    let Some(oracle) = oracle() else { return };
    let r = Repos::new(&oracle);
    build(&r);
    let theirs = r.base.join("theirs");
    r.svn(
        &r.base,
        &["checkout", "-q", &r.url(&r.main, "trunk"), "theirs"],
    );
    let sb = Sandbox::new();
    let (ours, _) = our_checkout(&r, &sb, "ours").await;
    for wc in [&theirs, &ours] {
        set_externals(&r, wc, "sub", "^/lib/sub local\n^/lib/a.txt local-file\n");
    }
    r.svn(&theirs, &["update", "-q"]);
    our_update(&r, &ours).await;
    assert_same_externals(&oracle, &theirs, &ours, "update with local definitions");
}

#[tokio::test(flavor = "multi_thread")]
async fn ignoring_externals_matches_ignore_externals() {
    let Some(oracle) = oracle() else { return };
    let r = Repos::new(&oracle);
    build(&r);
    let theirs = r.base.join("theirs");
    r.svn(
        &r.base,
        &[
            "checkout",
            "-q",
            "--ignore-externals",
            &r.url(&r.main, "trunk"),
            "theirs",
        ],
    );
    let sb = Sandbox::new();
    let (main, _) = r.fakes();
    let ours = sb.path("ours");
    let sum = checkout(
        &ctx(),
        &Repos::trunk(&main),
        &ours,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    assert!(sum.externals.is_empty());
    assert_same_externals(&oracle, &theirs, &ours, "checkout --ignore-externals");
    r.svn(&theirs, &["update", "-q", "--ignore-externals"]);
    update(
        &ctx(),
        &Repos::trunk(&main),
        &ours,
        None,
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect("update");
    assert_same_externals(&oracle, &theirs, &ours, "update --ignore-externals");
    let _ = RelPath::root();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_older_svn_reads_our_externals() {
    // Format 31 is shared by svn 1.8 to 1.15; run with OPENSVN_SVN_BIN pointing at an older
    // svn (1.14 here) to check it accepts the rows we write.
    let Some(oracle) = Oracle::resolve(opensvn_svn_oracle::MinVersion::V1_14).expect("oracle")
    else {
        return;
    };
    let r = Repos::new(&oracle);
    build(&r);
    let sb = Sandbox::new();
    let (ours, _) = our_checkout(&r, &sb, "ours").await;
    let status = r.svn(&ours, &["status"]);
    for line in [
        "X       ext",
        "X       other",
        "    X   fext",
        "    X   sub/pfext",
    ] {
        assert!(
            status.lines().any(|l| l == line),
            "{line} missing in:\n{status}"
        );
    }
    r.svn(&ours, &["cleanup"]);
    let info = r.svn(&ours, &["info", "fext", "ext/a.txt"]);
    assert!(info.contains("lib/f.txt"), "{info}");
    assert!(
        r.svn(&ours, &["propget", "svn:externals", "."])
            .contains("^/lib ext")
    );
}

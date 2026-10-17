//! An update that edits a node the working copy moved away, against svn 1.15: svn records the
//! tree conflict "local moved away, incoming edit" on the move's source, with both ends of the
//! move in the conflict (`(tree () moved-away edited <src> <dst>)`), updates the source's BASE
//! and leaves the destination as it was. The resolution then works like svn's:
//!
//! - `mine-conflict` carries the incoming changes to the destination (texts merged into local
//!   edits, with conflict markers named like svn's `<file>.tmp`, `.2.tmp`, `.3.tmp`; properties
//!   merged; files and folders added and deleted), and bases the destination on the new
//!   revision;
//! - `working` keeps the destination as it is and breaks the move (a copy and a deletion);
//! - `theirs-conflict` is refused, as svn refuses it, and nothing changes.
//!
//! An incoming delete of a moved-away node is "local moved away, incoming delete" and breaks
//! the move like svn. Files, NODES rows (with the move columns), ACTUAL rows (conflicts
//! included) and `svn status` must be identical after the update and after the resolution.
//!
//! Skipped when no svn 1.14 or newer is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};

use common::oracle::*;
use common::*;
use svn_client::local::{self, ResolveChoice, ResolveScope};
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth, Revnum};
use svn_wc::db::WcDb;

/// NODES rows with the move columns (the oracle's snapshot leaves them out).
fn move_rows(wc: &Path) -> Vec<String> {
    let db = WcDb::open(wc).expect("open");
    db.with_read(|c| {
        let mut stmt = c
            .prepare(
                "SELECT local_relpath, op_depth, presence, moved_here, moved_to FROM nodes \
                 ORDER BY local_relpath, op_depth",
            )
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| {
                Ok(format!(
                    "{}|{}|{}|{:?}|{:?}",
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })
            .expect("query");
        Ok(rows.map(|r| r.expect("row")).collect())
    })
    .expect("read")
}

/// Read-only working files (svn leaves its conflict markers writable).
fn read_only(root: &Path) -> Vec<String> {
    tree_snapshot(root)
        .into_keys()
        .filter(|rel| {
            std::fs::symlink_metadata(root.join(rel))
                .is_ok_and(|m| m.is_file() && m.permissions().readonly())
        })
        .collect()
}

fn same(oracle: &opensvn_svn_oracle::Oracle, theirs: &Path, ours: &Path, what: &str) {
    assert_same(oracle, theirs, ours, what);
    assert_eq!(
        move_rows(ours),
        move_rows(theirs),
        "{what}: move columns differ"
    );
    assert_eq!(
        read_only(ours),
        read_only(theirs),
        "{what}: read-only files differ"
    );
}

/// r1: `d/x.txt`, `d/y.txt`, `d/sub/s.txt`, `k.txt`.
fn build(repo: &Repo) -> PathBuf {
    let seed = repo.checkout("seed");
    write(&seed.join("d/x.txt"), "x\n");
    write(&seed.join("d/y.txt"), "y\n");
    write(&seed.join("d/sub/s.txt"), "s\n");
    write(&seed.join("k.txt"), "k\n");
    repo.svn(&seed, &["add", "-q", "d", "k.txt"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    seed
}

/// What r2 changes.
#[derive(Clone, Copy)]
enum Incoming {
    /// `d/x.txt` edited.
    EditFile,
    /// `d/x.txt` and `d/sub/s.txt` edited, `d/new.txt` added, `d/y.txt` deleted, a property
    /// on `d/sub`, a new folder `d/nd` with a file.
    EditDir,
    /// `d/x.txt` deleted.
    DeleteFile,
    /// `d` deleted.
    DeleteDir,
    /// A property on `d/x.txt` only.
    PropFile,
    /// `d/x.txt` edited, `d/new.txt` added, `d/y.txt` deleted.
    Mixed,
    /// `d/new.txt` added, `d/y.txt` deleted, `d/sub/s.txt` edited.
    AddDeleteNested,
    /// `d/sub` deleted.
    DeleteSub,
    /// A property on `d` and on `d/sub`.
    PropDirs,
}

fn commit_incoming(repo: &Repo, seed: &Path, what: Incoming) {
    match what {
        Incoming::EditFile => write(&seed.join("d/x.txt"), "x\nincoming\n"),
        Incoming::EditDir => {
            write(&seed.join("d/x.txt"), "x\nincoming\n");
            write(&seed.join("d/sub/s.txt"), "s\nincoming\n");
            write(&seed.join("d/new.txt"), "new\n");
            write(&seed.join("d/nd/n.txt"), "n\n");
            repo.svn(seed, &["add", "-q", "d/new.txt", "d/nd"]);
            repo.svn(seed, &["rm", "-q", "d/y.txt"]);
            repo.svn(seed, &["propset", "-q", "color", "red", "d/sub"]);
        }
        Incoming::DeleteFile => {
            repo.svn(seed, &["rm", "-q", "d/x.txt"]);
        }
        Incoming::DeleteDir => {
            repo.svn(seed, &["rm", "-q", "d"]);
        }
        Incoming::PropFile => {
            repo.svn(seed, &["propset", "-q", "color", "blue", "d/x.txt"]);
        }
        Incoming::Mixed => {
            write(&seed.join("d/x.txt"), "x\nincoming\n");
            write(&seed.join("d/new.txt"), "new\n");
            repo.svn(seed, &["add", "-q", "d/new.txt"]);
            repo.svn(seed, &["rm", "-q", "d/y.txt"]);
        }
        Incoming::AddDeleteNested => {
            write(&seed.join("d/new.txt"), "new\n");
            write(&seed.join("d/sub/s.txt"), "s\nincoming\n");
            repo.svn(seed, &["add", "-q", "d/new.txt"]);
            repo.svn(seed, &["rm", "-q", "d/y.txt"]);
        }
        Incoming::DeleteSub => {
            repo.svn(seed, &["rm", "-q", "d/sub"]);
        }
        Incoming::PropDirs => {
            repo.svn(seed, &["propset", "-q", "color", "red", "d", "d/sub"]);
        }
    }
    repo.svn(seed, &["commit", "-q", "-m", "r2"]);
}

/// A local change made the same way in both working copies.
enum Local {
    Svn(&'static [&'static str]),
    Write(&'static str, &'static str),
}

struct Case {
    name: &'static str,
    incoming: Incoming,
    local: &'static [Local],
    /// Path of the move source to resolve, `svn resolve --accept` word and engine choice.
    resolve: Option<(&'static str, &'static str, ResolveChoice)>,
}

async fn run(case: &Case) {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    let seed = build(&repo);
    let theirs = repo.base().join("svnwc");
    repo.svn(
        &repo.base(),
        &[
            "checkout",
            "-q",
            &format!("{}@1", repo.url),
            theirs.to_str().unwrap(),
        ],
    );
    let sb = Sandbox::new();
    let ours = sb.path("wc");
    checkout(
        &ctx(),
        &repo.fake(None),
        &ours,
        Some(Revnum(1)),
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    for l in case.local {
        for wc in [&theirs, &ours] {
            match l {
                Local::Svn(args) => {
                    repo.svn(wc, args);
                }
                Local::Write(path, text) => write(&wc.join(path), text),
            }
        }
    }
    commit_incoming(&repo, &seed, case.incoming);
    repo.svn(&theirs, &["update", "-q"]);
    update(
        &ctx(),
        &repo.fake(None),
        &ours,
        None,
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect("update");
    same(&oracle, &theirs, &ours, &format!("{}: update", case.name));
    let Some((path, accept, choice)) = case.resolve else {
        return;
    };
    let mut c = oracle.svn();
    c.args(["resolve", "--accept", accept, path])
        .current_dir(&theirs);
    let svn_ok = c.output().expect("svn resolve").status.success();
    let db = open_db(&ours);
    db.set_lock_owner(svn_wc::db::LockOwner {
        store: std::sync::Arc::new(svn_types::MemoryLockOwnership::for_current_process("boot")),
        pid: std::process::id(),
        boot_id: "boot".to_owned(),
    });
    let ours_result = local::resolve(
        &db,
        &CancelToken::new(),
        &rp(path),
        false,
        ResolveScope::ALL,
        choice,
    );
    drop(db);
    assert_eq!(
        ours_result.is_ok(),
        svn_ok,
        "{}: svn resolve --accept {accept} {} but the engine {:?}",
        case.name,
        if svn_ok { "succeeds" } else { "fails" },
        ours_result
    );
    same(
        &oracle,
        &theirs,
        &ours,
        &format!("{}: resolve --accept {accept}", case.name),
    );
}

macro_rules! case {
    ($test:ident, $case:expr) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $test() {
            run(&$case).await;
        }
    };
}

const MOVE_FILE: Local = Local::Svn(&["mv", "-q", "d/x.txt", "moved.txt"]);
const MOVE_DIR: Local = Local::Svn(&["mv", "-q", "d", "d2"]);

case!(
    moved_file_edit_conflicts,
    Case {
        name: "file edit",
        incoming: Incoming::EditFile,
        local: &[MOVE_FILE],
        resolve: None,
    }
);
case!(
    moved_file_edit_mine_conflict_clean,
    Case {
        name: "file edit, mine-conflict, clean destination",
        incoming: Incoming::EditFile,
        local: &[MOVE_FILE],
        resolve: Some(("d/x.txt", "mine-conflict", ResolveChoice::MineConflict)),
    }
);
case!(
    moved_file_edit_mine_conflict_text_conflict,
    Case {
        name: "file edit, mine-conflict, edited destination",
        incoming: Incoming::EditFile,
        local: &[MOVE_FILE, Local::Write("moved.txt", "x\nlocal\n")],
        resolve: Some(("d/x.txt", "mine-conflict", ResolveChoice::MineConflict)),
    }
);
case!(
    moved_file_edit_mine_conflict_merges,
    Case {
        name: "file edit, mine-conflict, merged destination",
        incoming: Incoming::EditFile,
        local: &[MOVE_FILE, Local::Write("moved.txt", "local\nx\n")],
        resolve: Some(("d/x.txt", "mine-conflict", ResolveChoice::MineConflict)),
    }
);
case!(
    moved_file_prop_mine_conflict,
    Case {
        name: "file property, mine-conflict",
        incoming: Incoming::PropFile,
        local: &[
            MOVE_FILE,
            Local::Svn(&["propset", "-q", "mine", "1", "moved.txt"])
        ],
        resolve: Some(("d/x.txt", "mine-conflict", ResolveChoice::MineConflict)),
    }
);
case!(
    moved_file_prop_mine_conflict_prop_conflict,
    Case {
        name: "file property, mine-conflict, conflicting local value",
        incoming: Incoming::PropFile,
        local: &[
            MOVE_FILE,
            Local::Svn(&["propset", "-q", "color", "green", "moved.txt"])
        ],
        resolve: Some(("d/x.txt", "mine-conflict", ResolveChoice::MineConflict)),
    }
);
case!(
    moved_file_edit_working_breaks_the_move,
    Case {
        name: "file edit, working",
        incoming: Incoming::EditFile,
        local: &[MOVE_FILE, Local::Write("moved.txt", "x\nlocal\n")],
        resolve: Some(("d/x.txt", "working", ResolveChoice::Working)),
    }
);
case!(
    moved_file_edit_theirs_conflict_is_refused,
    Case {
        name: "file edit, theirs-conflict",
        incoming: Incoming::EditFile,
        local: &[MOVE_FILE, Local::Write("moved.txt", "x\nlocal\n")],
        resolve: Some(("d/x.txt", "theirs-conflict", ResolveChoice::TheirsConflict)),
    }
);
case!(
    moved_dir_edit_conflicts,
    Case {
        name: "folder edits",
        incoming: Incoming::EditDir,
        local: &[MOVE_DIR],
        resolve: None,
    }
);
case!(
    moved_dir_edit_mine_conflict,
    Case {
        name: "folder edits, mine-conflict",
        incoming: Incoming::EditDir,
        local: &[MOVE_DIR, Local::Write("d2/x.txt", "x\nlocal\n")],
        resolve: Some(("d", "mine-conflict", ResolveChoice::MineConflict)),
    }
);
case!(
    moved_dir_edit_mine_conflict_clean,
    Case {
        name: "folder edits, mine-conflict, clean destination",
        incoming: Incoming::EditDir,
        local: &[MOVE_DIR],
        resolve: Some(("d", "mine-conflict", ResolveChoice::MineConflict)),
    }
);
case!(
    moved_dir_edit_working,
    Case {
        name: "folder edits, working",
        incoming: Incoming::EditDir,
        local: &[MOVE_DIR, Local::Write("d2/x.txt", "x\nlocal\n")],
        resolve: Some(("d", "working", ResolveChoice::Working)),
    }
);
case!(
    moved_file_delete_conflicts,
    Case {
        name: "file delete",
        incoming: Incoming::DeleteFile,
        local: &[MOVE_FILE],
        resolve: None,
    }
);
case!(
    moved_dir_delete_conflicts,
    Case {
        name: "folder delete",
        incoming: Incoming::DeleteDir,
        local: &[MOVE_DIR],
        resolve: None,
    }
);
case!(
    moved_dir_mine_conflict_with_local_obstacles,
    Case {
        name: "folder, mine-conflict: local edit vs delete, unversioned vs add, delete vs edit",
        incoming: Incoming::Mixed,
        local: &[
            MOVE_DIR,
            Local::Write("d2/y.txt", "y\nlocal\n"),
            Local::Write("d2/new.txt", "unversioned\n"),
            Local::Svn(&["rm", "-q", "d2/x.txt"]),
        ],
        resolve: Some(("d", "mine-conflict", ResolveChoice::MineConflict)),
    }
);
case!(
    moved_dir_mine_conflict_with_local_adds_and_deletes,
    Case {
        name: "folder, mine-conflict: add vs add, delete vs delete, deleted folder vs edit",
        incoming: Incoming::AddDeleteNested,
        local: &[
            MOVE_DIR,
            Local::Write("d2/new.txt", "mine\n"),
            Local::Svn(&["add", "-q", "d2/new.txt"]),
            Local::Svn(&["rm", "-q", "d2/y.txt", "d2/sub"]),
        ],
        resolve: Some(("d", "mine-conflict", ResolveChoice::MineConflict)),
    }
);
case!(
    moved_dir_mine_conflict_keeps_an_edited_folder_the_server_deleted,
    Case {
        name: "folder, mine-conflict: edit inside a folder the server deleted",
        incoming: Incoming::DeleteSub,
        local: &[MOVE_DIR, Local::Write("d2/sub/s.txt", "s\nlocal\n")],
        resolve: Some(("d", "mine-conflict", ResolveChoice::MineConflict)),
    }
);
case!(
    moved_dir_mine_conflict_removes_a_clean_folder_the_server_deleted,
    Case {
        name: "folder, mine-conflict: clean folder the server deleted",
        incoming: Incoming::DeleteSub,
        local: &[MOVE_DIR],
        resolve: Some(("d", "mine-conflict", ResolveChoice::MineConflict)),
    }
);
case!(
    moved_dir_props_mine_conflict,
    Case {
        name: "folder properties, mine-conflict, one conflicting",
        incoming: Incoming::PropDirs,
        local: &[
            MOVE_DIR,
            Local::Svn(&["propset", "-q", "color", "blue", "d2/sub"])
        ],
        resolve: Some(("d", "mine-conflict", ResolveChoice::MineConflict)),
    }
);
case!(
    moved_file_edit_base_is_refused,
    Case {
        name: "file edit, base",
        incoming: Incoming::EditFile,
        local: &[MOVE_FILE],
        resolve: Some(("d/x.txt", "base", ResolveChoice::Base)),
    }
);

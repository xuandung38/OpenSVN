//! Work item codec and runner tests (in-memory queue, real temp directory).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use super::*;
use crate::pristine::{Md5Digest, PristineRecord, PristineStore};
use crate::skel::Skel;
use crate::translate::flags::SyncInput;
use crate::translate::utc_offset_zero;

fn text(item: &WorkItem) -> String {
    String::from_utf8(item.encode()).unwrap()
}

#[test]
fn encodes_exactly_what_svn_builders_produce() {
    // Integers are explicit-length atoms ("1 0" = length 1, bytes "0"), paths implicit ones.
    let cases: Vec<(WorkItem, &str)> = vec![
        (
            WorkItem::FileInstall {
                relpath: "trunk/a.c".into(),
                use_commit_times: false,
                record_fileinfo: true,
                source: None,
            },
            "(file-install trunk/a.c 1 0 1 1)",
        ),
        (
            WorkItem::FileInstall {
                relpath: "a.c".into(),
                use_commit_times: true,
                record_fileinfo: false,
                source: Some(".svn/tmp/x".into()),
            },
            "(file-install a.c 1 1 1 0 10 .svn/tmp/x)",
        ),
        (
            WorkItem::FileRemove {
                relpath: "a.c".into(),
            },
            "(file-remove a.c)",
        ),
        (
            WorkItem::FileMove {
                src: "a".into(),
                dst: "b".into(),
            },
            "(file-move a b)",
        ),
        (
            WorkItem::FileTranslate {
                local: "f".into(),
                src: "s".into(),
                dst: "d".into(),
            },
            "(file-translate f s d)",
        ),
        (
            WorkItem::SyncFileFlags {
                relpath: "f".into(),
            },
            "(sync-file-flags f)",
        ),
        (
            WorkItem::PrejInstall {
                relpath: "f".into(),
                legacy_conflict: None,
            },
            "(prej-install f)",
        ),
        (
            WorkItem::DirInstall {
                relpath: "d".into(),
            },
            "(dir-install d)",
        ),
        (
            WorkItem::DirRemove {
                relpath: "d".into(),
                recursive: false,
            },
            "(dir-remove d)",
        ),
        (
            WorkItem::DirRemove {
                relpath: "d".into(),
                recursive: true,
            },
            "(dir-remove d 1 1)",
        ),
        (
            WorkItem::FileCommit {
                relpath: "f".into(),
            },
            "(file-commit f)",
        ),
        (WorkItem::PostUpgrade, "(postupgrade)"),
    ];
    for (item, expected) in cases {
        assert_eq!(text(&item), expected);
        assert_eq!(
            WorkItem::decode(expected.as_bytes()).unwrap(),
            item,
            "{expected}"
        );
    }
}

#[test]
fn decodes_legacy_shapes() {
    // Old file-commit items carried two more values that svn ignores.
    assert_eq!(
        WorkItem::decode(b"(file-commit f 1 0 1 0)").unwrap(),
        WorkItem::FileCommit {
            relpath: "f".into()
        }
    );
    // base-remove: modern (rev, flag) and pre-1.7 (keep flag).
    assert_eq!(
        WorkItem::decode(b"(base-remove f 2 -1 1 0)").unwrap(),
        WorkItem::BaseRemove {
            relpath: "f".into(),
            value: -1,
            second: Some(0)
        }
    );
    assert_eq!(
        WorkItem::decode(b"(base-remove f 1 1)").unwrap(),
        WorkItem::BaseRemove {
            relpath: "f".into(),
            value: 1,
            second: None
        }
    );
    assert_eq!(
        WorkItem::decode(b"(record-fileinfo f)").unwrap(),
        WorkItem::RecordFileinfo {
            relpath: "f".into(),
            mtime: None
        }
    );
    assert_eq!(
        WorkItem::decode(b"(record-fileinfo f 16 1700000000000000)").unwrap(),
        WorkItem::RecordFileinfo {
            relpath: "f".into(),
            mtime: Some(1_700_000_000_000_000)
        }
    );
    // prej-install with the 1.7/1.8 conflict skel keeps it verbatim.
    let item = WorkItem::decode(b"(prej-install f (conflict x))").unwrap();
    assert!(matches!(
        &item,
        WorkItem::PrejInstall {
            legacy_conflict: Some(Skel::List(_)),
            ..
        }
    ));
    assert_eq!(text(&item), "(prej-install f (conflict x))");
}

#[test]
fn extra_trailing_arguments_are_ignored_like_svn() {
    assert_eq!(
        WorkItem::decode(b"(file-remove a b c)").unwrap(),
        WorkItem::FileRemove {
            relpath: "a".into()
        }
    );
    assert_eq!(
        WorkItem::decode(b"(postupgrade x)").unwrap(),
        WorkItem::PostUpgrade
    );
    assert_eq!(
        WorkItem::decode(b"(file-move a b c)").unwrap(),
        WorkItem::FileMove {
            src: "a".into(),
            dst: "b".into()
        }
    );
    // Real item captured from svn 1.15.0 after an interrupted `svn propset svn:executable`.
    assert_eq!(
        WorkItem::decode(b"(sync-file-flags a.txt)").unwrap(),
        WorkItem::SyncFileFlags {
            relpath: "a.txt".into()
        }
    );
}

#[test]
fn rejects_unknown_and_malformed_items() {
    assert!(matches!(
        WorkItem::decode(b"(tmp-set-text-conflict-markers f)"),
        Err(WorkQueueError::UnknownItem(op)) if op == "tmp-set-text-conflict-markers"
    ));
    assert!(matches!(
        WorkItem::decode(b"(frobnicate x)"),
        Err(WorkQueueError::UnknownItem(_))
    ));
    for bad in [
        &b"(file-remove)"[..],
        b"(file-install a 1 0)",
        b"(file-install a x y)",
        b"(file-move a)",
        b"()",
        b"file-remove",
        b"((file-remove a))",
        b"(dir-remove d q)",
        b"(base-remove f)",
    ] {
        assert!(
            matches!(WorkItem::decode(bad), Err(WorkQueueError::Malformed { .. })),
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
    let e: svn_types::SvnError = WorkQueueError::UnknownItem("x".into()).into();
    assert_eq!(e.code, svn_types::ErrorCode::WcNeedsCleanup);
    assert_eq!(e.svn_code, Some(155_009));
}

// ---------------------------------------------------------------- runner

#[derive(Default)]
struct MemQueue {
    items: Vec<(u64, Vec<u8>)>,
    fileinfo: Vec<FileInfoRecord>,
}

impl MemQueue {
    fn with(items: &[WorkItem]) -> Self {
        Self {
            items: items
                .iter()
                .enumerate()
                .map(|(i, it)| (i as u64 + 1, it.encode()))
                .collect(),
            fileinfo: Vec::new(),
        }
    }
}

impl WorkQueueStore for MemQueue {
    fn next_item(
        &mut self,
        completed: Option<u64>,
        fileinfo: &[FileInfoRecord],
    ) -> Result<Option<(u64, Vec<u8>)>, WorkQueueError> {
        if let Some(id) = completed {
            self.items.retain(|(i, _)| *i != id);
        }
        self.fileinfo.extend_from_slice(fileinfo);
        Ok(self.items.first().cloned())
    }
}

#[derive(Default)]
struct FakeOps {
    nodes: BTreeMap<String, NodeInstallInfo>,
    /// Actual props per relpath; defaults to the node's pristine props when absent.
    actual: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    sync: Option<SyncInput>,
    calls: Vec<String>,
}

impl WorkingCopyOps for FakeOps {
    fn install_info(&self, relpath: &str) -> Result<NodeInstallInfo, WorkQueueError> {
        Ok(self.nodes.get(relpath).cloned().unwrap_or_default())
    }
    fn actual_props(&self, relpath: &str) -> Result<BTreeMap<String, Vec<u8>>, WorkQueueError> {
        Ok(self.actual.get(relpath).cloned().unwrap_or_else(|| {
            self.nodes
                .get(relpath)
                .map(|n| n.props.clone())
                .unwrap_or_default()
        }))
    }
    fn sync_flags_input(&self, _: &str) -> Result<Option<SyncInput>, WorkQueueError> {
        Ok(self.sync)
    }
    fn write_prop_reject_file(
        &mut self,
        relpath: &str,
        legacy: Option<&Skel>,
    ) -> Result<(), WorkQueueError> {
        self.calls
            .push(format!("prej {relpath} {}", legacy.is_some()));
        Ok(())
    }
    fn file_commit(&mut self, relpath: &str) -> Result<(), WorkQueueError> {
        self.calls.push(format!("commit {relpath}"));
        Ok(())
    }
    fn base_remove(&mut self, relpath: &str, rev: Option<i64>) -> Result<(), WorkQueueError> {
        self.calls.push(format!("base-remove {relpath} {rev:?}"));
        Ok(())
    }
    fn post_upgrade(&mut self) -> Result<(), WorkQueueError> {
        self.calls.push("postupgrade".into());
        Ok(())
    }
}

struct Wc {
    dir: tempfile::TempDir,
    pristine: PristineStore<MemPristineIndex>,
}

#[derive(Default)]
struct MemPristineIndex(BTreeMap<crate::pristine::Sha1Digest, PristineRecord>);

impl crate::pristine::PristineIndex for MemPristineIndex {
    fn record(&mut self, r: &PristineRecord) -> Result<(), crate::pristine::PristineError> {
        self.0.entry(r.sha1).or_insert_with(|| r.clone());
        Ok(())
    }
    fn lookup(
        &self,
        s: &crate::pristine::Sha1Digest,
    ) -> Result<Option<PristineRecord>, crate::pristine::PristineError> {
        Ok(self.0.get(s).cloned())
    }
    fn unreferenced(
        &self,
    ) -> Result<Vec<crate::pristine::Sha1Digest>, crate::pristine::PristineError> {
        Ok(Vec::new())
    }
    fn forget_if_unreferenced(
        &mut self,
        _: &crate::pristine::Sha1Digest,
    ) -> Result<bool, crate::pristine::PristineError> {
        Ok(false)
    }
}

fn wc() -> Wc {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".svn")).unwrap();
    let pristine = PristineStore::open(
        dir.path().join(".svn"),
        crate::pristine::PristineMode::Writable,
        MemPristineIndex::default(),
    );
    Wc { dir, pristine }
}

fn run(wc: &Wc, ops: &mut FakeOps, queue: &mut MemQueue) -> Result<usize, WorkQueueError> {
    let config = RunnerConfig {
        wc_root: wc.dir.path().to_path_buf(),
        tz: &utc_offset_zero,
        cancelled: None,
    };
    WorkQueueRunner::new(config, ops, &wc.pristine).run(queue)
}

fn node(sha: crate::pristine::Sha1Digest, props: &[(&str, &str)]) -> NodeInstallInfo {
    NodeInstallInfo {
        checksum: Some(sha),
        props: props
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
            .collect(),
        changed_date_micros: Some(1_000_000_000_000_000),
        changed_rev: Some(7),
        changed_author: Some("al".into()),
        repos_relpath: "trunk/a b.txt".into(),
        repos_root_url: "http://h/repo".into(),
        is_added: false,
        has_lock: false,
    }
}

fn install(wc: &mut Wc, content: &[u8]) -> crate::pristine::Sha1Digest {
    wc.pristine
        .install(&mut Cursor::new(content.to_vec()), None, None)
        .unwrap()
        .sha1
}

#[test]
fn file_install_translates_and_records_fileinfo() {
    let mut w = wc();
    let sha = install(&mut w, b"$Rev$ $URL$\nline\n");
    let mut ops = FakeOps::default();
    ops.nodes.insert(
        "a.txt".into(),
        node(
            sha,
            &[
                ("svn:keywords", "Rev URL"),
                ("svn:eol-style", "CRLF"),
                ("svn:executable", "*"),
            ],
        ),
    );
    let mut q = MemQueue::with(&[WorkItem::FileInstall {
        relpath: "a.txt".into(),
        use_commit_times: true,
        record_fileinfo: true,
        source: None,
    }]);
    assert_eq!(run(&w, &mut ops, &mut q).unwrap(), 1);
    assert!(q.items.is_empty());

    let path = w.dir.path().join("a.txt");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"$Rev: 7 $ $URL: http://h/repo/trunk/a%20b.txt $\r\nline\r\n"
    );
    let md = std::fs::metadata(&path).unwrap();
    // use_commit_times: mtime is the last changed date.
    assert_eq!(
        md.modified().unwrap(),
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(md.permissions().mode() & 0o100, 0, "svn:executable");
    }
    assert_eq!(
        q.fileinfo,
        vec![FileInfoRecord {
            relpath: "a.txt".into(),
            last_mod_time_micros: 1_000_000_000_000_000,
            translated_size: md.len() as i64,
        }]
    );
    // No temp files left behind.
    assert_eq!(
        std::fs::read_dir(w.dir.path().join(".svn/tmp"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn file_install_needs_lock_makes_the_file_read_only_unless_locked_or_added() {
    for (has_lock, is_added, read_only) in [
        (false, false, true),
        (true, false, false),
        (false, true, false),
    ] {
        let mut w = wc();
        let sha = install(&mut w, b"x");
        let mut n = node(sha, &[("svn:needs-lock", "*")]);
        n.has_lock = has_lock;
        n.is_added = is_added;
        let mut ops = FakeOps::default();
        ops.nodes.insert("f".into(), n);
        let mut q = MemQueue::with(&[WorkItem::FileInstall {
            relpath: "f".into(),
            use_commit_times: false,
            record_fileinfo: false,
            source: None,
        }]);
        run(&w, &mut ops, &mut q).unwrap();
        let ro = std::fs::metadata(w.dir.path().join("f"))
            .unwrap()
            .permissions()
            .readonly();
        assert_eq!(ro, read_only, "lock={has_lock} added={is_added}");
        assert!(q.fileinfo.is_empty());
    }
}

#[test]
fn file_install_replaces_an_existing_read_only_file() {
    let mut w = wc();
    let sha = install(&mut w, b"new");
    let path = w.dir.path().join("f");
    std::fs::write(&path, b"old").unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms).unwrap();
    let mut ops = FakeOps::default();
    ops.nodes.insert("f".into(), node(sha, &[]));
    let mut q = MemQueue::with(&[WorkItem::FileInstall {
        relpath: "f".into(),
        use_commit_times: false,
        record_fileinfo: false,
        source: None,
    }]);
    run(&w, &mut ops, &mut q).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
}

#[test]
fn file_install_from_explicit_source_and_special() {
    let mut w = wc();
    let sha = install(&mut w, b"unused");
    std::fs::create_dir_all(w.dir.path().join("sub")).unwrap();
    std::fs::write(w.dir.path().join("src.tmp"), b"link dest/x").unwrap();
    let mut ops = FakeOps::default();
    ops.nodes.insert(
        "sub/l".into(),
        node(sha, &[("svn:special", "*"), ("svn:eol-style", "CRLF")]),
    );
    let mut q = MemQueue::with(&[WorkItem::FileInstall {
        relpath: "sub/l".into(),
        use_commit_times: false,
        record_fileinfo: true,
        source: Some("src.tmp".into()),
    }]);
    run(&w, &mut ops, &mut q).unwrap();
    let p = w.dir.path().join("sub/l");
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_link(&p).unwrap(),
        std::path::Path::new("dest/x")
    );
    #[cfg(not(unix))]
    assert_eq!(std::fs::read(&p).unwrap(), b"link dest/x");
    // Special files are never translated even with svn:eol-style.
    assert!(q.fileinfo.len() <= 1);
}

#[test]
fn file_install_without_checksum_or_pristine_fails_and_keeps_the_item() {
    let w = wc();
    let mut ops = FakeOps::default();
    ops.nodes.insert("f".into(), NodeInstallInfo::default());
    let item = WorkItem::FileInstall {
        relpath: "f".into(),
        use_commit_times: false,
        record_fileinfo: false,
        source: None,
    };
    let mut q = MemQueue::with(std::slice::from_ref(&item));
    let err = run(&w, &mut ops, &mut q).unwrap_err();
    assert!(
        matches!(err, WorkQueueError::ItemFailed { id: 1, .. }),
        "{err}"
    );
    assert_eq!(q.items.len(), 1, "a failed item stays queued");
    let svn: svn_types::SvnError = err.into();
    assert_eq!(svn.svn_code, Some(155_009));
}

#[test]
fn simple_filesystem_items() {
    let w = wc();
    let root = w.dir.path();
    std::fs::create_dir_all(root.join("d/inner")).unwrap();
    std::fs::write(root.join("d/inner/f"), b"x").unwrap();
    std::fs::write(root.join("m1"), b"moved").unwrap();
    std::fs::write(root.join("gone"), b"x").unwrap();
    let mut ops = FakeOps::default();
    let mut q = MemQueue::with(&[
        WorkItem::FileRemove {
            relpath: "gone".into(),
        },
        WorkItem::FileRemove {
            relpath: "never-existed".into(),
        },
        WorkItem::FileMove {
            src: "m1".into(),
            dst: "new/m2".into(),
        },
        WorkItem::FileMove {
            src: "m1".into(),
            dst: "new/m2".into(),
        },
        WorkItem::DirInstall {
            relpath: "made/deep".into(),
        },
        WorkItem::DirRemove {
            relpath: "d".into(),
            recursive: false,
        },
        WorkItem::DirRemove {
            relpath: "d".into(),
            recursive: true,
        },
        WorkItem::DirRemove {
            relpath: "d".into(),
            recursive: true,
        },
        WorkItem::DirRemove {
            relpath: "m1".into(),
            recursive: false,
        },
    ]);
    assert_eq!(run(w_ref(&w), &mut ops, &mut q).unwrap(), 9);
    assert!(!root.join("gone").exists());
    assert_eq!(std::fs::read(root.join("new/m2")).unwrap(), b"moved");
    assert!(root.join("made/deep").is_dir());
    assert!(!root.join("d").exists());
}

fn w_ref(w: &Wc) -> &Wc {
    w
}

#[test]
fn file_translate_uses_the_properties_of_the_local_file() {
    let mut w = wc();
    let sha = install(&mut w, b"unused");
    std::fs::write(w.dir.path().join("src"), b"a\r\n$Rev$\n").unwrap();
    let mut ops = FakeOps::default();
    ops.nodes.insert(
        "local".into(),
        node(sha, &[("svn:eol-style", "LF"), ("svn:keywords", "Rev")]),
    );
    let mut q = MemQueue::with(&[WorkItem::FileTranslate {
        local: "local".into(),
        src: "src".into(),
        dst: "dst".into(),
    }]);
    run(&w, &mut ops, &mut q).unwrap();
    assert_eq!(
        std::fs::read(w.dir.path().join("dst")).unwrap(),
        b"a\n$Rev: 7 $\n"
    );
    assert!(w.dir.path().join("src").exists());
}

#[cfg(unix)]
#[test]
fn sync_flags_and_database_items_call_through() {
    use std::os::unix::fs::PermissionsExt;
    let w = wc();
    let path = w.dir.path().join("f");
    std::fs::write(&path, b"x").unwrap();
    let mut ops = FakeOps {
        sync: Some(SyncInput {
            status_is_normal: true,
            needs_lock_working: false,
            needs_lock_pristine: false,
            has_lock: false,
            executable: true,
        }),
        ..FakeOps::default()
    };
    let mut q = MemQueue::with(&[
        WorkItem::SyncFileFlags {
            relpath: "f".into(),
        },
        WorkItem::PrejInstall {
            relpath: "f".into(),
            legacy_conflict: None,
        },
        WorkItem::FileCommit {
            relpath: "f".into(),
        },
        WorkItem::BaseRemove {
            relpath: "f".into(),
            value: 5,
            second: Some(0),
        },
        WorkItem::BaseRemove {
            relpath: "g".into(),
            value: -1,
            second: Some(0),
        },
        WorkItem::BaseRemove {
            relpath: "h".into(),
            value: 1,
            second: None,
        },
        WorkItem::PostUpgrade,
    ]);
    run(&w, &mut ops, &mut q).unwrap();
    assert_ne!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o100,
        0
    );
    assert_eq!(
        ops.calls,
        [
            "prej f false",
            "commit f",
            "base-remove f Some(5)",
            "base-remove g None",
            "base-remove h None",
            "postupgrade"
        ]
    );
}

#[test]
fn record_fileinfo_legacy_item_sets_mtime_and_records() {
    let w = wc();
    let path = w.dir.path().join("f");
    std::fs::write(&path, b"abc").unwrap();
    let mut ops = FakeOps::default();
    let mut q = MemQueue::with(&[WorkItem::RecordFileinfo {
        relpath: "f".into(),
        mtime: Some(1_500_000_000_000_000),
    }]);
    run(&w, &mut ops, &mut q).unwrap();
    assert_eq!(q.fileinfo[0].last_mod_time_micros, 1_500_000_000_000_000);
    assert_eq!(q.fileinfo[0].translated_size, 3);
}

#[test]
fn unknown_item_stops_the_run_and_nothing_after_it_runs() {
    let w = wc();
    std::fs::write(w.dir.path().join("a"), b"x").unwrap();
    std::fs::write(w.dir.path().join("b"), b"x").unwrap();
    let mut ops = FakeOps::default();
    let mut q = MemQueue::with(&[WorkItem::FileRemove {
        relpath: "a".into(),
    }]);
    q.items.push((2, b"(future-op x)".to_vec()));
    q.items.push((
        3,
        WorkItem::FileRemove {
            relpath: "b".into(),
        }
        .encode(),
    ));
    let err = run(&w, &mut ops, &mut q).unwrap_err();
    let WorkQueueError::ItemFailed { id, source, .. } = err else {
        panic!("unexpected {err:?}")
    };
    assert_eq!(id, 2);
    assert!(matches!(*source, WorkQueueError::UnknownItem(_)));
    assert!(!w.dir.path().join("a").exists());
    assert!(
        w.dir.path().join("b").exists(),
        "items behind an unknown one must not run"
    );
    assert_eq!(q.items.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [2, 3]);
}

#[test]
fn items_cannot_escape_the_working_copy() {
    let w = wc();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("victim"), b"x").unwrap();
    for bad in [
        "../victim",
        "/etc/passwd",
        "a/../../x",
        "C:/x",
        ".svn/../../x",
    ] {
        let mut ops = FakeOps::default();
        let mut q = MemQueue::with(&[WorkItem::FileRemove {
            relpath: bad.into(),
        }]);
        let err = run(&w, &mut ops, &mut q).unwrap_err();
        let WorkQueueError::ItemFailed { source, .. } = err else {
            panic!("{bad}")
        };
        assert!(
            matches!(*source, WorkQueueError::UnsafePath(_)),
            "{bad}: {source}"
        );
    }
    #[cfg(unix)]
    {
        // A symlinked ancestor inside the WC must not be followed either.
        std::os::unix::fs::symlink(outside.path(), w.dir.path().join("link")).unwrap();
        let mut ops = FakeOps::default();
        let mut q = MemQueue::with(&[WorkItem::FileRemove {
            relpath: "link/victim".into(),
        }]);
        assert!(run(&w, &mut ops, &mut q).is_err());
        assert!(outside.path().join("victim").exists());
    }
}

#[test]
fn cancellation_stops_between_items() {
    let w = wc();
    std::fs::write(w.dir.path().join("a"), b"x").unwrap();
    std::fs::write(w.dir.path().join("b"), b"x").unwrap();
    let mut ops = FakeOps::default();
    let mut q = MemQueue::with(&[
        WorkItem::FileRemove {
            relpath: "a".into(),
        },
        WorkItem::FileRemove {
            relpath: "b".into(),
        },
    ]);
    let polls = std::cell::Cell::new(0);
    let cancel = || {
        polls.set(polls.get() + 1);
        polls.get() >= 2
    };
    let config = RunnerConfig {
        wc_root: w.dir.path().to_path_buf(),
        tz: &utc_offset_zero,
        cancelled: Some(&cancel),
    };
    let err = WorkQueueRunner::new(config, &mut ops, &w.pristine)
        .run(&mut q)
        .unwrap_err();
    let svn: svn_types::SvnError = err.into();
    assert_eq!(svn.code, svn_types::ErrorCode::Cancelled);
    assert!(!w.dir.path().join("a").exists());
    assert!(w.dir.path().join("b").exists());
}

#[test]
fn pristine_source_reads_installed_text() {
    let mut w = wc();
    let sha = install(&mut w, b"abc");
    let mut buf = String::new();
    PristineSource::open_text(&w.pristine, &sha)
        .unwrap()
        .read_to_string(&mut buf)
        .unwrap();
    assert_eq!(buf, "abc");
    let _ = Md5Digest([0; 16]);
}

// ------------------------------------------------- admin tmp operands, props source, moves

use super::paths::{resolve_admin_tmp, resolve_operand, resolve_wc_path};

#[test]
fn admin_tmp_resolver_accepts_exactly_one_component() {
    let w = wc();
    std::fs::create_dir_all(w.dir.path().join(".svn/tmp")).unwrap();
    let ok = resolve_admin_tmp(w.dir.path(), ".svn/tmp/svn-bCQBU2").unwrap();
    assert_eq!(ok, w.dir.path().join(".svn/tmp/svn-bCQBU2"));
    for bad in [
        ".svn/tmp/",
        ".svn/tmp",
        ".svn/tmp/a/b",
        ".svn/tmp/..",
        ".svn/tmp/.",
        ".svn/tmp/../wc.db",
        ".svn/wc.db",
        ".svn/pristine/ab/x",
        ".svn/tmp/x:stream",
        ".svn/tmp/x\\y",
        ".svn/tmp/CON",
        ".svn/tmp/trailing.",
        ".svn/tmpx/f",
        ".svn/tmp//f",
        "/.svn/tmp/f",
        "a/.svn/tmp/f",
        ".SVN/tmp/f",
    ] {
        assert!(resolve_admin_tmp(w.dir.path(), bad).is_err(), "{bad}");
    }
    // Ordinary operands still refuse the administrative directory.
    assert!(resolve_wc_path(w.dir.path(), ".svn/tmp/svn-x").is_err());
    assert!(resolve_wc_path(w.dir.path(), ".svn/wc.db").is_err());
    assert!(resolve_operand(w.dir.path(), ".svn/wc.db").is_err());
    assert!(resolve_operand(w.dir.path(), ".svn/tmp/svn-x").is_ok());
    assert!(resolve_operand(w.dir.path(), "dir/file").is_ok());
}

#[cfg(unix)]
#[test]
fn admin_tmp_resolver_refuses_symlinked_adm_or_tmp() {
    let outside = tempfile::tempdir().unwrap();
    let w = wc();
    std::fs::remove_dir_all(w.dir.path().join(".svn/tmp")).ok();
    std::os::unix::fs::symlink(outside.path(), w.dir.path().join(".svn/tmp")).unwrap();
    assert!(resolve_admin_tmp(w.dir.path(), ".svn/tmp/f").is_err());

    let w2 = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), w2.path().join(".svn")).unwrap();
    std::fs::create_dir_all(outside.path().join("tmp")).unwrap();
    assert!(resolve_admin_tmp(w2.path(), ".svn/tmp/f").is_err());

    let w3 = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(w3.path().join(".svn")).unwrap();
    assert!(
        resolve_admin_tmp(w3.path(), ".svn/tmp/f").is_err(),
        "missing .svn/tmp"
    );
}

fn queue_fixture(case: &str) -> Vec<Vec<u8>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/compat/scenarios/wc/workqueue/queue.txt");
    let text = std::fs::read_to_string(&path).unwrap();
    text.lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(c, _)| *c == case)
        .map(|(_, item)| item.chars().map(|c| c as u8).collect())
        .collect()
}

/// The queue svn 1.15.0 left behind when `svn revert` was interrupted (recorded by
/// `tests/compat/scenarios/wc/workqueue/generate.py`): install from, then remove, a temporary
/// file inside `.svn/tmp`. It must run to completion.
#[test]
fn interrupted_svn_revert_queue_completes() {
    let items = queue_fixture("interrupted_revert");
    assert_eq!(items.len(), 2);
    let decoded: Vec<_> = items.iter().map(|i| WorkItem::decode(i).unwrap()).collect();
    assert!(
        matches!(&decoded[0], WorkItem::FileInstall { relpath, source: Some(src), .. }
        if relpath == "b.txt" && src.starts_with(".svn/tmp/svn-"))
    );
    assert!(
        matches!(&decoded[1], WorkItem::FileRemove { relpath } if relpath.starts_with(".svn/tmp/svn-"))
    );
    for (item, original) in decoded.iter().zip(&items) {
        assert_eq!(
            &item.encode(),
            original,
            "re-encoding must be byte identical"
        );
    }

    let mut w = wc();
    let sha = install(&mut w, b"not used: the explicit source wins\n");
    let WorkItem::FileInstall {
        source: Some(tmp_rel),
        ..
    } = &decoded[0]
    else {
        unreachable!()
    };
    std::fs::create_dir_all(w.dir.path().join(".svn/tmp")).unwrap();
    std::fs::write(w.dir.path().join(tmp_rel), b"base\n").unwrap();
    std::fs::write(w.dir.path().join("b.txt"), b"modified\n").unwrap();
    let mut ops = FakeOps::default();
    ops.nodes.insert("b.txt".into(), node(sha, &[]));
    let mut q = MemQueue {
        items: items
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, b)| (i as u64 + 1, b))
            .collect(),
        fileinfo: Vec::new(),
    };
    assert_eq!(run(&w, &mut ops, &mut q).unwrap(), 2);
    assert_eq!(
        std::fs::read(w.dir.path().join("b.txt")).unwrap(),
        b"base\n"
    );
    assert!(!w.dir.path().join(tmp_rel).exists());
    assert_eq!(
        q.fileinfo.len(),
        1,
        "record_fileinfo was set on the install"
    );
}

#[test]
fn interrupted_svn_update_queue_removes_the_deleted_file() {
    let items = queue_fixture("interrupted_update_delete");
    assert_eq!(items, [b"(file-remove c.txt)".to_vec()]);
    let w = wc();
    std::fs::write(w.dir.path().join("c.txt"), b"c").unwrap();
    let mut ops = FakeOps::default();
    let mut q = MemQueue {
        items: vec![(1, items[0].clone())],
        fileinfo: Vec::new(),
    };
    run(&w, &mut ops, &mut q).unwrap();
    assert!(!w.dir.path().join("c.txt").exists());
}

#[test]
fn admin_paths_are_refused_where_they_are_not_documented_as_temporary() {
    let mut w = wc();
    let sha = install(&mut w, b"x");
    let mut ops = FakeOps::default();
    ops.nodes.insert(".svn/tmp/dest".into(), node(sha, &[]));
    for bad in [
        WorkItem::FileInstall {
            relpath: ".svn/tmp/dest".into(),
            use_commit_times: false,
            record_fileinfo: false,
            source: None,
        },
        WorkItem::DirInstall {
            relpath: ".svn/tmp/d".into(),
        },
        WorkItem::DirRemove {
            relpath: ".svn/tmp".into(),
            recursive: true,
        },
        WorkItem::FileRemove {
            relpath: ".svn/wc.db".into(),
        },
        WorkItem::FileMove {
            src: ".svn/tmp/a".into(),
            dst: ".svn/wc.db".into(),
        },
    ] {
        let mut q = MemQueue::with(std::slice::from_ref(&bad));
        let err = run(&w, &mut ops, &mut q).unwrap_err();
        let WorkQueueError::ItemFailed { source, .. } = err else {
            panic!("{bad:?}")
        };
        assert!(
            matches!(*source, WorkQueueError::UnsafePath(_)),
            "{bad:?}: {source}"
        );
    }
    assert!(w.dir.path().join(".svn").is_dir());
}

#[test]
fn file_install_uses_pristine_props_and_file_translate_uses_actual_props() {
    let mut w = wc();
    let sha = install(&mut w, b"a\nb\n");
    std::fs::write(w.dir.path().join("src"), b"a\nb\n").unwrap();
    let mut ops = FakeOps::default();
    // Pristine says LF; the uncommitted local change says CRLF.
    ops.nodes
        .insert("f".into(), node(sha, &[("svn:eol-style", "LF")]));
    ops.actual.insert(
        "f".into(),
        [("svn:eol-style".to_string(), b"CRLF".to_vec())].into(),
    );
    let mut q = MemQueue::with(&[
        WorkItem::FileInstall {
            relpath: "f".into(),
            use_commit_times: false,
            record_fileinfo: false,
            source: None,
        },
        WorkItem::FileTranslate {
            local: "f".into(),
            src: "src".into(),
            dst: "g".into(),
        },
    ]);
    run(&w, &mut ops, &mut q).unwrap();
    assert_eq!(std::fs::read(w.dir.path().join("f")).unwrap(), b"a\nb\n");
    assert_eq!(
        std::fs::read(w.dir.path().join("g")).unwrap(),
        b"a\r\nb\r\n"
    );
}

#[test]
fn file_move_failures_other_than_cross_device_fail_the_item_and_keep_the_source() {
    let w = wc();
    std::fs::write(w.dir.path().join("src"), b"data").unwrap();
    std::fs::create_dir_all(w.dir.path().join("dstdir/inner")).unwrap();
    let mut ops = FakeOps::default();
    let mut q = MemQueue::with(&[WorkItem::FileMove {
        src: "src".into(),
        dst: "dstdir".into(),
    }]);
    let err = run(&w, &mut ops, &mut q).unwrap_err();
    assert!(matches!(err, WorkQueueError::ItemFailed { .. }));
    assert_eq!(
        std::fs::read(w.dir.path().join("src")).unwrap(),
        b"data",
        "no copy fallback"
    );
    assert!(w.dir.path().join("dstdir/inner").is_dir());
    assert_eq!(q.items.len(), 1);
}

#[cfg(unix)]
#[test]
fn file_move_replaces_a_symlink_at_the_destination_instead_of_writing_through_it() {
    let w = wc();
    let outside = tempfile::tempdir().unwrap();
    let victim = outside.path().join("victim");
    std::fs::write(&victim, b"keep").unwrap();
    std::fs::write(w.dir.path().join("src"), b"new").unwrap();
    std::os::unix::fs::symlink(&victim, w.dir.path().join("dst")).unwrap();
    let mut ops = FakeOps::default();
    let mut q = MemQueue::with(&[WorkItem::FileMove {
        src: "src".into(),
        dst: "dst".into(),
    }]);
    run(&w, &mut ops, &mut q).unwrap();
    assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
    assert_eq!(std::fs::read(w.dir.path().join("dst")).unwrap(), b"new");
    assert!(
        !std::fs::symlink_metadata(w.dir.path().join("dst"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

/// `copy_then_remove` is what a cross-device move falls back to; exercised directly because a
/// second device is not available in a unit test.
#[cfg(unix)]
#[test]
fn cross_device_copy_never_follows_symlinks() {
    let w = wc();
    let outside = tempfile::tempdir().unwrap();
    let victim = outside.path().join("victim");
    std::fs::write(&victim, b"keep").unwrap();
    std::fs::write(w.dir.path().join("src"), b"new").unwrap();
    std::os::unix::fs::symlink(&victim, w.dir.path().join("dst")).unwrap();
    super::runner::copy_then_remove_for_tests(&w.dir.path().join("src"), &w.dir.path().join("dst"))
        .unwrap();
    assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
    assert_eq!(std::fs::read(w.dir.path().join("dst")).unwrap(), b"new");
    assert!(!w.dir.path().join("src").exists());
    std::os::unix::fs::symlink(&victim, w.dir.path().join("lsrc")).unwrap();
    assert!(
        super::runner::copy_then_remove_for_tests(
            &w.dir.path().join("lsrc"),
            &w.dir.path().join("x")
        )
        .is_err()
    );
}

#[test]
fn special_content_is_streamed_with_a_capped_head() {
    let mut w = wc();
    // 300 KB of an unknown special type: written whole as a regular file.
    let mut big = b"unknown kind\n".to_vec();
    big.extend(std::iter::repeat_n(b'x', 300_000));
    let sha = install(&mut w, &big);
    let mut ops = FakeOps::default();
    ops.nodes
        .insert("s".into(), node(sha, &[("svn:special", "*")]));
    let mut q = MemQueue::with(&[WorkItem::FileInstall {
        relpath: "s".into(),
        use_commit_times: false,
        record_fileinfo: false,
        source: None,
    }]);
    run(&w, &mut ops, &mut q).unwrap();
    assert_eq!(std::fs::read(w.dir.path().join("s")).unwrap(), big);

    // A "link " line longer than the cap is rejected rather than buffered.
    #[cfg(unix)]
    {
        let mut long = b"link ".to_vec();
        long.extend(std::iter::repeat_n(
            b'a',
            crate::translate::special::MAX_SPECIAL_HEAD + 10,
        ));
        let sha = install(&mut w, &long);
        ops.nodes
            .insert("l".into(), node(sha, &[("svn:special", "*")]));
        let mut q = MemQueue::with(&[WorkItem::FileInstall {
            relpath: "l".into(),
            use_commit_times: false,
            record_fileinfo: false,
            source: None,
        }]);
        assert!(run(&w, &mut ops, &mut q).is_err());
    }
}

#[cfg(unix)]
#[test]
fn file_translate_treats_a_symlink_source_as_special() {
    let mut w = wc();
    let sha = install(&mut w, b"x");
    std::os::unix::fs::symlink("some/dest", w.dir.path().join("lsrc")).unwrap();
    let mut ops = FakeOps::default();
    ops.nodes.insert("local".into(), node(sha, &[]));
    let mut q = MemQueue::with(&[WorkItem::FileTranslate {
        local: "local".into(),
        src: "lsrc".into(),
        dst: "ldst".into(),
    }]);
    run(&w, &mut ops, &mut q).unwrap();
    assert_eq!(
        std::fs::read_link(w.dir.path().join("ldst")).unwrap(),
        std::path::Path::new("some/dest")
    );
}

/// svn requires every item to be re-runnable (`SVN_RUN_WORK_QUEUE_TWICE`): run each one twice.
#[test]
fn every_filesystem_item_is_idempotent() {
    let mut w = wc();
    let sha = install(&mut w, b"$Rev$\n");
    std::fs::write(w.dir.path().join("src"), b"x\n").unwrap();
    std::fs::write(w.dir.path().join("m1"), b"moved").unwrap();
    std::fs::create_dir_all(w.dir.path().join("rd/in")).unwrap();
    std::fs::write(w.dir.path().join("gone"), b"x").unwrap();
    let mut ops = FakeOps::default();
    ops.nodes
        .insert("f".into(), node(sha, &[("svn:keywords", "Rev")]));
    ops.sync = Some(SyncInput {
        status_is_normal: true,
        needs_lock_working: false,
        needs_lock_pristine: false,
        has_lock: false,
        executable: false,
    });
    let items = [
        WorkItem::FileInstall {
            relpath: "f".into(),
            use_commit_times: true,
            record_fileinfo: true,
            source: None,
        },
        WorkItem::FileTranslate {
            local: "f".into(),
            src: "src".into(),
            dst: "t".into(),
        },
        WorkItem::SyncFileFlags {
            relpath: "f".into(),
        },
        WorkItem::FileMove {
            src: "m1".into(),
            dst: "sub/m2".into(),
        },
        WorkItem::FileRemove {
            relpath: "gone".into(),
        },
        WorkItem::DirInstall {
            relpath: "made".into(),
        },
        WorkItem::DirRemove {
            relpath: "rd".into(),
            recursive: true,
        },
        WorkItem::RecordFileinfo {
            relpath: "f".into(),
            mtime: None,
        },
    ];
    let doubled: Vec<_> = items.iter().flat_map(|i| [i.clone(), i.clone()]).collect();
    let mut q = MemQueue::with(&doubled);
    assert_eq!(run(&w, &mut ops, &mut q).unwrap(), 16);
    assert_eq!(
        std::fs::read(w.dir.path().join("f")).unwrap(),
        b"$Rev: 7 $\n"
    );
    assert_eq!(
        std::fs::read(w.dir.path().join("sub/m2")).unwrap(),
        b"moved"
    );
    assert!(!w.dir.path().join("rd").exists() && w.dir.path().join("made").is_dir());
}

#[cfg(windows)]
mod windows_unverified {
    //! Never run in CI (no Windows CI): written to be checked on the first Windows run.
    use super::*;

    fn read_only(path: &std::path::Path) {
        let mut p = std::fs::metadata(path).unwrap().permissions();
        p.set_readonly(true);
        std::fs::set_permissions(path, p).unwrap();
    }

    #[test]
    fn read_only_files_can_be_removed_and_removed_recursively() {
        let w = wc();
        std::fs::create_dir_all(w.dir.path().join("d")).unwrap();
        for f in ["ro", "d/ro"] {
            std::fs::write(w.dir.path().join(f), b"x").unwrap();
            read_only(&w.dir.path().join(f));
        }
        let mut ops = FakeOps::default();
        let mut q = MemQueue::with(&[
            WorkItem::FileRemove {
                relpath: "ro".into(),
            },
            WorkItem::DirRemove {
                relpath: "d".into(),
                recursive: true,
            },
        ]);
        run(&w, &mut ops, &mut q).unwrap();
        assert!(!w.dir.path().join("ro").exists() && !w.dir.path().join("d").exists());
    }

    #[test]
    fn mtime_can_be_set_on_a_read_only_file() {
        let w = wc();
        let p = w.dir.path().join("f");
        std::fs::write(&p, b"abc").unwrap();
        read_only(&p);
        let mut ops = FakeOps::default();
        let mut q = MemQueue::with(&[WorkItem::RecordFileinfo {
            relpath: "f".into(),
            mtime: Some(1_500_000_000_000_000),
        }]);
        run(&w, &mut ops, &mut q).unwrap();
        assert_eq!(q.fileinfo[0].last_mod_time_micros, 1_500_000_000_000_000);
    }
}

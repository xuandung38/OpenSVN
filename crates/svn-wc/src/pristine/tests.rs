//! Pristine store tests with an in-memory index standing in for the `PRISTINE` table.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};

use super::*;
use svn_types::{ErrorCode, SvnError};

#[derive(Default)]
struct MemIndex {
    rows: BTreeMap<Sha1Digest, PristineRecord>,
    /// Rows referenced by NODES/ACTUAL_NODE (refcount > 0).
    referenced: BTreeSet<Sha1Digest>,
    /// Rows that become referenced right after `unreferenced()` returned (models a concurrent
    /// reference taken between the snapshot and the delete).
    reference_after_snapshot: BTreeSet<Sha1Digest>,
}

impl PristineIndex for MemIndex {
    fn record(&mut self, r: &PristineRecord) -> Result<(), PristineError> {
        self.rows.entry(r.sha1).or_insert_with(|| r.clone());
        Ok(())
    }
    fn lookup(&self, s: &Sha1Digest) -> Result<Option<PristineRecord>, PristineError> {
        Ok(self.rows.get(s).cloned())
    }
    fn unreferenced(&self) -> Result<Vec<Sha1Digest>, PristineError> {
        Ok(self
            .rows
            .keys()
            .filter(|k| !self.referenced.contains(k))
            .copied()
            .collect())
    }
    fn forget_if_unreferenced(&mut self, s: &Sha1Digest) -> Result<bool, PristineError> {
        if self.reference_after_snapshot.remove(s) {
            self.referenced.insert(*s);
        }
        if self.referenced.contains(s) {
            return Ok(false);
        }
        Ok(self.rows.remove(s).is_some())
    }
}

fn store() -> (tempfile::TempDir, PristineStore<MemIndex>) {
    let dir = tempfile::tempdir().unwrap();
    let adm = dir.path().join(".svn");
    std::fs::create_dir_all(&adm).unwrap();
    let st = PristineStore::open(adm, PristineMode::Writable, MemIndex::default());
    (dir, st)
}

// Well-known digests of "hello\n".
const HELLO_SHA1: &str = "f572d396fae9206628714fb2ce00f72e94f2258f";
const HELLO_MD5: &str = "b1946ac92492d2347c6235b4d2611184";

#[test]
fn install_stores_by_sha1_with_svn_layout() {
    let (_d, mut st) = store();
    let rec = st
        .install(&mut Cursor::new(b"hello\n"), None, None)
        .unwrap();
    assert_eq!(rec.sha1.to_hex(), HELLO_SHA1);
    assert_eq!(rec.md5.to_hex(), HELLO_MD5);
    assert_eq!(rec.size, 6);
    let path = st.path_of(&rec.sha1);
    assert!(path.ends_with(format!("pristine/f5/{HELLO_SHA1}.svn-base")));
    assert_eq!(std::fs::read(&path).unwrap(), b"hello\n");
    assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
    assert_eq!(st.index().rows.get(&rec.sha1), Some(&rec));
    // Temp area is left empty.
    assert_eq!(
        std::fs::read_dir(st.adm_dir.join("tmp")).unwrap().count(),
        0
    );
}

#[test]
fn svn_checksum_strings_roundtrip() {
    let s = Sha1Digest::from_hex(HELLO_SHA1).unwrap();
    assert_eq!(s.to_svn_string(), format!("$sha1${HELLO_SHA1}"));
    assert_eq!(Sha1Digest::from_svn_string(&s.to_svn_string()), Some(s));
    let m = Md5Digest::from_hex(HELLO_MD5).unwrap();
    assert_eq!(m.to_svn_string(), format!("$md5 ${HELLO_MD5}"));
    assert_eq!(Md5Digest::from_svn_string(&m.to_svn_string()), Some(m));
    assert_eq!(Sha1Digest::from_hex("zz"), None);
    assert_eq!(Sha1Digest::from_svn_string("$md5 $00"), None);
}

#[test]
fn expected_checksums_are_verified_and_nothing_is_left_behind() {
    let (_d, mut st) = store();
    let wrong = Sha1Digest([0; 20]);
    let err = st
        .install(&mut Cursor::new(b"hello\n"), Some(&wrong), None)
        .unwrap_err();
    assert!(matches!(err, PristineError::ChecksumMismatch { .. }));
    let wrong_md5 = Md5Digest([0; 16]);
    let err = st
        .install(&mut Cursor::new(b"hello\n"), None, Some(&wrong_md5))
        .unwrap_err();
    assert!(matches!(err, PristineError::ChecksumMismatch { .. }));
    assert!(st.index().rows.is_empty());
    assert!(!st.adm_dir.join("pristine").exists());
    assert_eq!(
        std::fs::read_dir(st.adm_dir.join("tmp")).unwrap().count(),
        0
    );
    // Correct expectations pass.
    let ok = Sha1Digest::from_hex(HELLO_SHA1).unwrap();
    st.install(
        &mut Cursor::new(b"hello\n"),
        Some(&ok),
        Md5Digest::from_hex(HELLO_MD5).as_ref(),
    )
    .unwrap();
}

#[test]
fn reinstalling_same_content_is_idempotent() {
    let (_d, mut st) = store();
    let a = st.install(&mut Cursor::new(b"same"), None, None).unwrap();
    let b = st.install(&mut Cursor::new(b"same"), None, None).unwrap();
    assert_eq!(a, b);
    assert_eq!(st.index().rows.len(), 1);
}

#[test]
fn empty_and_large_content() {
    let (_d, mut st) = store();
    let e = st.install(&mut Cursor::new(b""), None, None).unwrap();
    assert_eq!(e.size, 0);
    assert_eq!(e.sha1.to_hex(), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    let big = vec![7u8; 300_000];
    let b = st.install(&mut Cursor::new(&big), None, None).unwrap();
    assert_eq!(b.size, 300_000);
    let mut got = Vec::new();
    st.open_text(&b.sha1)
        .unwrap()
        .read_to_end(&mut got)
        .unwrap();
    assert_eq!(got, big);
    assert!(st.verify(&b.sha1).unwrap());
}

#[test]
fn open_reports_missing_and_dehydrated() {
    let (_d, mut st) = store();
    let sha = Sha1Digest::from_hex(HELLO_SHA1).unwrap();
    assert!(matches!(
        st.open_text(&sha),
        Err(PristineError::NotInstalled(_))
    ));
    // Row without file (dehydrated, format 32).
    st.index_mut().rows.insert(
        sha,
        PristineRecord {
            sha1: sha,
            md5: Md5Digest([0; 16]),
            size: 6,
            hydrated: false,
        },
    );
    assert!(matches!(
        st.open_text(&sha),
        Err(PristineError::NotHydrated(_))
    ));
    // Row says hydrated but file is gone.
    st.index_mut().rows.get_mut(&sha).unwrap().hydrated = true;
    assert!(matches!(
        st.open_text(&sha),
        Err(PristineError::NotInstalled(_))
    ));
}

#[test]
fn verify_detects_corruption() {
    let (_d, mut st) = store();
    let rec = st
        .install(&mut Cursor::new(b"payload"), None, None)
        .unwrap();
    let path = st.path_of(&rec.sha1);
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&path, perms).unwrap();
    std::fs::write(&path, b"tampered").unwrap();
    assert!(!st.verify(&rec.sha1).unwrap());
}

#[test]
fn gc_removes_only_unreferenced_texts() {
    let (_d, mut st) = store();
    let keep = st.install(&mut Cursor::new(b"keep"), None, None).unwrap();
    let drop = st.install(&mut Cursor::new(b"drop"), None, None).unwrap();
    // `keep` is referenced (e.g. only through an ACTUAL_NODE checksum column).
    st.index_mut().referenced.insert(keep.sha1);
    assert_eq!(st.remove_unreferenced().unwrap(), 1);
    assert!(st.path_of(&keep.sha1).exists());
    assert!(!st.path_of(&drop.sha1).exists());
    assert!(st.index().rows.contains_key(&keep.sha1));
    assert!(!st.index().rows.contains_key(&drop.sha1));
    // Missing file for an unreferenced row is tolerated and the row dropped.
    st.index_mut().referenced.clear();
    std::fs::remove_file(st.path_of(&keep.sha1)).unwrap_or(());
    assert_eq!(st.remove_unreferenced().unwrap(), 1);
    assert!(st.index().rows.is_empty());
}

#[test]
fn gc_keeps_a_text_that_gained_a_reference_after_the_snapshot() {
    let (_d, mut st) = store();
    let rec = st.install(&mut Cursor::new(b"racy"), None, None).unwrap();
    st.index_mut().reference_after_snapshot.insert(rec.sha1);
    assert_eq!(st.remove_unreferenced().unwrap(), 0);
    assert!(
        st.path_of(&rec.sha1).exists(),
        "a referenced pristine must never be deleted"
    );
    assert!(st.index().rows.contains_key(&rec.sha1));
    assert!(st.verify(&rec.sha1).unwrap());
}

#[test]
fn install_overwrites_an_orphan_file_that_has_no_row() {
    let (_d, mut st) = store();
    let sha = Sha1Digest::from_hex("f572d396fae9206628714fb2ce00f72e94f2258f").unwrap();
    let path = st.path_of(&sha);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"hel").unwrap(); // truncated leftover of a crashed install
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms).unwrap();
    let rec = st
        .install(&mut Cursor::new(b"hello\n"), None, None)
        .unwrap();
    assert_eq!(rec.sha1, sha);
    assert_eq!(std::fs::read(&path).unwrap(), b"hello\n");
    assert!(st.verify(&sha).unwrap());
}

#[test]
fn install_trusts_the_row_and_keeps_the_stored_file() {
    let (_d, mut st) = store();
    let rec = st
        .install(&mut Cursor::new(b"payload"), None, None)
        .unwrap();
    let path = st.path_of(&rec.sha1);
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    let again = st
        .install(&mut Cursor::new(b"payload"), None, None)
        .unwrap();
    assert_eq!(again, rec);
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        before
    );
}

#[test]
fn dehydrated_row_cannot_be_reinstalled_in_this_version() {
    let (_d, mut st) = store();
    let sha = Sha1Digest::from_hex("f572d396fae9206628714fb2ce00f72e94f2258f").unwrap();
    st.index_mut().rows.insert(
        sha,
        PristineRecord {
            sha1: sha,
            md5: Md5Digest([0; 16]),
            size: 6,
            hydrated: false,
        },
    );
    let err = st
        .install(&mut Cursor::new(b"hello\n"), None, None)
        .unwrap_err();
    assert!(matches!(err, PristineError::RehydrateUnsupported(_)));
    assert!(!st.path_of(&sha).exists());
}

#[test]
fn read_only_mode_reads_but_refuses_writes() {
    let (d, mut st) = store();
    let rec = st.install(&mut Cursor::new(b"data"), None, None).unwrap();
    let index = std::mem::take(st.index_mut());
    let mut ro = PristineStore::open(d.path().join(".svn"), PristineMode::ReadOnly, index);
    assert!(ro.verify(&rec.sha1).unwrap());
    let err = ro
        .install(&mut Cursor::new(b"more"), None, None)
        .unwrap_err();
    assert!(matches!(err, PristineError::ReadOnlyFormat));
    assert!(matches!(
        ro.remove_unreferenced(),
        Err(PristineError::ReadOnlyFormat)
    ));
    let svn: SvnError = PristineError::ReadOnlyFormat.into();
    assert_eq!(svn.code, ErrorCode::WcFormatUnsupported);
}

#[test]
fn verify_detects_truncation_by_size() {
    let (_d, mut st) = store();
    let rec = st
        .install(&mut Cursor::new(b"payload"), None, None)
        .unwrap();
    st.index_mut().rows.get_mut(&rec.sha1).unwrap().size += 1;
    assert!(!st.verify(&rec.sha1).unwrap());
}

#[test]
fn errors_convert_to_svn_errors() {
    let e: SvnError = PristineError::ChecksumMismatch {
        expected: "a".into(),
        actual: "b".into(),
    }
    .into();
    assert_eq!((e.code, e.svn_code), (ErrorCode::Corrupt, Some(200_014)));
    let e: SvnError = PristineError::NotInstalled(Sha1Digest([0; 20])).into();
    assert_eq!((e.code, e.svn_code), (ErrorCode::Corrupt, Some(155_017)));
    let e: SvnError = PristineError::NotHydrated(Sha1Digest([0; 20])).into();
    assert_eq!((e.code, e.svn_code), (ErrorCode::PristineNotHydrated, None));
    let e: SvnError = PristineError::index("boom").into();
    assert_eq!(e.code, ErrorCode::Internal);
}

#[cfg(unix)]
#[test]
fn rename_failure_keeps_its_classification() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, mut st) = store();
    let sha = Sha1Digest::from_hex("f572d396fae9206628714fb2ce00f72e94f2258f").unwrap();
    // The fan-out directory exists but is not writable, so the rename fails with EACCES.
    let dir = st.path_of(&sha).parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let probe = dir.join("probe");
    let running_as_root = std::fs::write(&probe, b"x").is_ok();
    let result = st.install(&mut Cursor::new(b"hello\n"), None, None);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    if running_as_root {
        return; // permissions are not enforced for root
    }
    let err = result.unwrap_err();
    assert!(matches!(err, PristineError::Svn(_)), "{err:?}");
    assert!(
        st.index().rows.is_empty(),
        "no row for a text that was not stored"
    );
    assert_eq!(
        std::fs::read_dir(st.adm_dir.join("tmp")).unwrap().count(),
        0,
        "temp file cleaned up"
    );
    let svn: SvnError = err.into();
    assert_eq!(
        svn.code,
        ErrorCode::PermissionDenied,
        "the I/O classification must survive: {svn:?}"
    );
}

fn hello_record() -> PristineRecord {
    PristineRecord {
        sha1: Sha1Digest::from_hex(HELLO_SHA1).unwrap(),
        md5: Md5Digest::from_hex(HELLO_MD5).unwrap(),
        size: 6,
        hydrated: true,
    }
}

#[test]
fn batch_stage_places_texts_without_rows() {
    let (_d, st) = store();
    let batch = st.batch().unwrap();
    let rec = batch
        .stage(&mut Cursor::new(b"hello\n"), None, None)
        .unwrap();
    assert_eq!(rec, hello_record());
    let path = st.path_of(&rec.sha1);
    assert_eq!(batch.path_of(&rec.sha1), path);
    assert_eq!(std::fs::read(&path).unwrap(), b"hello\n");
    assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
    assert!(st.index().rows.is_empty(), "the caller records the rows");
    batch.sync_dirs(&[rec]).unwrap();
    // A wrong checksum leaves nothing behind.
    let wrong = Sha1Digest([7; 20]);
    assert!(matches!(
        batch.stage(&mut Cursor::new(b"other\n"), Some(&wrong), None),
        Err(PristineError::ChecksumMismatch { .. })
    ));
    assert_eq!(
        std::fs::read_dir(st.adm_dir.join("tmp")).unwrap().count(),
        0
    );
}

#[test]
fn batch_adopt_moves_a_checked_file_into_the_store() {
    let (d, st) = store();
    let batch = st.batch().unwrap();
    let src = d.path().join("copy.txt");
    std::fs::write(&src, b"hello\n").unwrap();
    let want = hello_record();
    // Checked before anything moves: a wrong expectation keeps the file where it is.
    let wrong_md5 = Md5Digest([1; 16]);
    assert!(matches!(
        batch.adopt(&src, &want.sha1, &wrong_md5),
        Err(PristineError::ChecksumMismatch { .. })
    ));
    assert!(src.is_file());
    let rec = batch.adopt(&src, &want.sha1, &want.md5).unwrap();
    assert_eq!(rec, want);
    assert!(!src.exists(), "renamed, not copied");
    let path = st.path_of(&rec.sha1);
    assert_eq!(std::fs::read(&path).unwrap(), b"hello\n");
    assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
    // A missing source is an I/O error the caller can recognize.
    match batch.adopt(&src, &want.sha1, &want.md5) {
        Err(PristineError::Io { source, .. }) => {
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn batch_adopt_trusted_checks_the_size_only() {
    let (d, st) = store();
    let batch = st.batch().unwrap();
    let src = d.path().join("copy.txt");
    std::fs::write(&src, b"hello!\n").unwrap();
    assert!(matches!(
        batch.adopt_trusted(&src, &hello_record()),
        Err(PristineError::ChecksumMismatch { .. })
    ));
    assert!(src.is_file());
    std::fs::write(&src, b"hello\n").unwrap();
    let rec = batch.adopt_trusted(&src, &hello_record()).unwrap();
    assert_eq!(rec, hello_record());
    assert!(!src.exists());
    assert_eq!(std::fs::read(st.path_of(&rec.sha1)).unwrap(), b"hello\n");
}

#[test]
fn batch_needs_a_writable_store() {
    let dir = tempfile::tempdir().unwrap();
    let st = PristineStore::open(
        dir.path().join(".svn"),
        PristineMode::ReadOnly,
        MemIndex::default(),
    );
    assert!(matches!(st.batch(), Err(PristineError::ReadOnlyFormat)));
}

/// A temp file holding `content` in the store's directory, with its record (as the update
/// editor hands it to `place`).
fn placed_text(
    st: &PristineStore<MemIndex>,
    content: &[u8],
) -> (tempfile::TempPath, PristineRecord) {
    use sha1::Digest as _;
    let mut f = tempfile::NamedTempFile::new_in(&st.adm_dir).unwrap();
    f.write_all(content).unwrap();
    let record = PristineRecord {
        sha1: Sha1Digest(sha1::Sha1::digest(content).into()),
        md5: Md5Digest(md5::Md5::digest(content).into()),
        size: content.len() as u64,
        hydrated: true,
    };
    (f.into_temp_path(), record)
}

#[cfg(unix)]
fn inode(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().ino()
}

#[cfg(unix)]
#[test]
fn place_never_replaces_a_text_placed_but_not_committed() {
    let (_d, mut st) = store();
    let mut uncommitted = HashSet::new();
    let (first, rec) = placed_text(&st, b"same text\n");
    assert!(
        st.place(first, &rec, false, &mut uncommitted)
            .unwrap()
            .is_placed()
    );
    assert!(uncommitted.contains(&rec.sha1));
    let dest = st.path_of(&rec.sha1);
    let (ino, mtime) = (
        inode(&dest),
        std::fs::metadata(&dest).unwrap().modified().unwrap(),
    );

    // Same text again before the first row is committed (the next batch of the same drive):
    // the stored file stays the one the earlier barrier covers; the new copy is dropped.
    let (second, again) = placed_text(&st, b"same text\n");
    let second_path = second.to_path_buf();
    let Placement::AlreadyStored(spare) =
        st.place(second, &again, false, &mut uncommitted).unwrap()
    else {
        panic!("placed over an uncommitted text");
    };
    assert_eq!(inode(&dest), ino, "renamed over an uncommitted text");
    assert_eq!(std::fs::metadata(&dest).unwrap().modified().unwrap(), mtime);
    assert!(second_path.exists(), "the copy is handed back");
    drop(spare);
    assert!(!second_path.exists(), "the dropped copy is removed");
    assert_eq!(std::fs::read(&dest).unwrap(), b"same text\n");

    // Committed: the row answers first and nothing is renamed either.
    st.index.record(&rec).unwrap();
    let (third, again) = placed_text(&st, b"same text\n");
    assert!(
        !st.place(third, &again, false, &mut uncommitted)
            .unwrap()
            .is_placed()
    );
    assert_eq!(inode(&dest), ino);
}

#[cfg(unix)]
#[test]
fn place_replaces_an_orphan_of_an_earlier_run() {
    let (_d, mut st) = store();
    let (file, rec) = placed_text(&st, b"hello\n");
    let dest = st.path_of(&rec.sha1);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(&dest, b"hel").unwrap(); // partial leftover: no row, not placed by this drive
    let ino = inode(&dest);
    let mut uncommitted = HashSet::new();
    assert!(
        st.place(file, &rec, false, &mut uncommitted)
            .unwrap()
            .is_placed()
    );
    assert_ne!(inode(&dest), ino);
    assert_eq!(std::fs::read(&dest).unwrap(), b"hello\n");

    // A text in the set whose file has gone missing is placed again.
    std::fs::remove_file(&dest).unwrap();
    let (file, rec) = placed_text(&st, b"hello\n");
    assert!(
        st.place(file, &rec, false, &mut uncommitted)
            .unwrap()
            .is_placed()
    );
    assert_eq!(std::fs::read(&dest).unwrap(), b"hello\n");
}

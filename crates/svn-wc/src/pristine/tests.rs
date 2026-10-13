//! Pristine store tests with an in-memory index standing in for the `PRISTINE` table.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};

use super::*;

#[derive(Default)]
struct MemIndex {
    rows: BTreeMap<Sha1Digest, PristineRecord>,
    /// Rows referenced by NODES/ACTUAL_NODE (refcount > 0).
    referenced: BTreeSet<Sha1Digest>,
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
    fn forget(&mut self, s: &Sha1Digest) -> Result<(), PristineError> {
        self.rows.remove(s);
        Ok(())
    }
}

fn store() -> (tempfile::TempDir, PristineStore<MemIndex>) {
    let dir = tempfile::tempdir().unwrap();
    let adm = dir.path().join(".svn");
    std::fs::create_dir_all(&adm).unwrap();
    let st = PristineStore::new(adm, MemIndex::default());
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
    st.open(&b.sha1).unwrap().read_to_end(&mut got).unwrap();
    assert_eq!(got, big);
    assert!(st.verify(&b.sha1).unwrap());
}

#[test]
fn open_reports_missing_and_dehydrated() {
    let (_d, mut st) = store();
    let sha = Sha1Digest::from_hex(HELLO_SHA1).unwrap();
    assert!(matches!(st.open(&sha), Err(PristineError::NotInstalled(_))));
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
    assert!(matches!(st.open(&sha), Err(PristineError::NotHydrated(_))));
    // Row says hydrated but file is gone.
    st.index_mut().rows.get_mut(&sha).unwrap().hydrated = true;
    assert!(matches!(st.open(&sha), Err(PristineError::NotInstalled(_))));
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

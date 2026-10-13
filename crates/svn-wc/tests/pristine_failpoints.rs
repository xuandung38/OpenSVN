//! Crash injection for the pristine store. Lives in its own test binary because failpoint
//! configuration is process-global and would leak into unrelated parallel tests.
#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::io::Cursor;

use svn_wc::pristine::{
    PristineError, PristineIndex, PristineMode, PristineRecord, PristineStore, Sha1Digest,
};

#[derive(Default)]
struct Rows(BTreeMap<Sha1Digest, PristineRecord>);

impl PristineIndex for Rows {
    fn record(&mut self, r: &PristineRecord) -> Result<(), PristineError> {
        self.0.entry(r.sha1).or_insert_with(|| r.clone());
        Ok(())
    }
    fn lookup(&self, s: &Sha1Digest) -> Result<Option<PristineRecord>, PristineError> {
        Ok(self.0.get(s).cloned())
    }
    fn unreferenced(&self) -> Result<Vec<Sha1Digest>, PristineError> {
        Ok(Vec::new())
    }
    fn forget_if_unreferenced(&mut self, _: &Sha1Digest) -> Result<bool, PristineError> {
        Ok(false)
    }
}

/// Crash between the temp file and the rename: nothing is stored, no row, no temp file left,
/// and the same install succeeds afterwards.
#[test]
fn failpoint_before_rename_leaves_no_row_and_no_file() {
    let scenario = fail::FailScenario::setup();
    let dir = tempfile::tempdir().unwrap();
    let adm = dir.path().join(".svn");
    let mut st = PristineStore::open(&adm, PristineMode::Writable, Rows::default());
    let sha = Sha1Digest::from_hex("f572d396fae9206628714fb2ce00f72e94f2258f").unwrap();

    fail::cfg("pristine.install.before_rename", "return").unwrap();
    let err = st
        .install(&mut Cursor::new(b"hello\n"), None, None)
        .unwrap_err();
    assert!(matches!(err, PristineError::Index(_)));
    assert!(st.index().0.is_empty());
    assert!(!st.path_of(&sha).exists());
    assert_eq!(std::fs::read_dir(adm.join("tmp")).unwrap().count(), 0);

    fail::remove("pristine.install.before_rename");
    st.install(&mut Cursor::new(b"hello\n"), None, None)
        .unwrap();
    assert!(st.verify(&sha).unwrap());
    scenario.teardown();
}

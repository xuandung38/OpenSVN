//! `safe_join` and `write_file_replacing` behavior on a real file system.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;

use svn_types::{ErrorCode, RelPath, safe_join, write_file_replacing};

fn rel(s: &str) -> RelPath {
    RelPath::new(s).expect("valid rel path")
}

#[test]
fn joins_existing_and_missing_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(dir.path().join("a/b")).expect("mkdir");
    fs::write(dir.path().join("a/b/f"), b"x").expect("write");
    assert_eq!(
        safe_join(dir.path(), &rel("a/b/f")).expect("existing"),
        dir.path().join("a/b/f")
    );
    assert_eq!(
        safe_join(dir.path(), &rel("a/new/deeper/f")).expect("missing"),
        dir.path().join("a/new/deeper/f")
    );
    assert_eq!(
        safe_join(dir.path(), &RelPath::root()).expect("root"),
        dir.path()
    );
}

#[test]
fn rejects_admin_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = safe_join(dir.path(), &rel("a/.SVN/wc.db")).expect_err("must fail");
    assert_eq!(err.code, ErrorCode::UnsafePath);
}

#[test]
fn rejects_file_in_the_middle_of_the_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("f"), b"x").expect("write");
    let err = safe_join(dir.path(), &rel("f/child")).expect_err("must fail");
    assert_eq!(err.code, ErrorCode::UnsafePath);
}

#[cfg(unix)]
mod symlinks {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn rejects_symlink_in_intermediate_component() {
        let outside = tempfile::tempdir().expect("outside");
        let dir = tempfile::tempdir().expect("wc");
        symlink(outside.path(), dir.path().join("link")).expect("symlink");
        let err = safe_join(dir.path(), &rel("link/evil")).expect_err("must fail");
        assert_eq!(err.code, ErrorCode::UnsafePath);
        // Also deeper in the chain.
        fs::create_dir(dir.path().join("real")).expect("mkdir");
        symlink(outside.path(), dir.path().join("real/up")).expect("symlink");
        assert!(safe_join(dir.path(), &rel("real/up/x/y")).is_err());
    }

    #[test]
    fn allows_symlink_as_last_component_and_root() {
        let outside = tempfile::tempdir().expect("outside");
        let dir = tempfile::tempdir().expect("wc");
        symlink(outside.path(), dir.path().join("last")).expect("symlink");
        assert_eq!(
            safe_join(dir.path(), &rel("last")).expect("last"),
            dir.path().join("last")
        );
        // The root itself may be a symlink (it is the trust anchor).
        let holder = tempfile::tempdir().expect("holder");
        let root_link = holder.path().join("wc-link");
        symlink(dir.path(), &root_link).expect("symlink");
        assert!(safe_join(&root_link, &rel("a/b")).is_ok());
    }

    #[test]
    fn write_replaces_a_final_symlink_instead_of_following_it() {
        let outside = tempfile::tempdir().expect("outside");
        let victim = outside.path().join("victim");
        fs::write(&victim, b"original").expect("victim");
        let dir = tempfile::tempdir().expect("wc");
        let dest = safe_join(dir.path(), &rel("file")).expect("join");
        symlink(&victim, &dest).expect("symlink");

        write_file_replacing(&dest, b"payload").expect("write");

        assert_eq!(
            fs::read(&victim).expect("victim"),
            b"original",
            "target of the link must be untouched"
        );
        let md = fs::symlink_metadata(&dest).expect("lstat");
        assert!(
            md.file_type().is_file(),
            "destination must now be a regular file"
        );
        assert_eq!(fs::read(&dest).expect("dest"), b"payload");
        let leftovers: Vec<_> = fs::read_dir(dir.path()).expect("ls").collect();
        assert_eq!(leftovers.len(), 1, "no temporary file may remain");
    }
}

#[test]
fn write_replaces_regular_file_atomically() {
    let dir = tempfile::tempdir().expect("wc");
    let dest = dir.path().join("f");
    fs::write(&dest, b"old").expect("old");
    write_file_replacing(&dest, b"new").expect("write");
    assert_eq!(fs::read(&dest).expect("read"), b"new");
}

#[test]
fn write_fails_cleanly_without_parent() {
    let dir = tempfile::tempdir().expect("wc");
    let err = write_file_replacing(&dir.path().join("missing/f"), b"x").expect_err("must fail");
    assert_eq!(err.code, ErrorCode::PathNotFound);
}

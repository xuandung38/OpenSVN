//! Marker naming and creation tests.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::workqueue::WorkItem;

fn wc() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".svn/tmp")).unwrap();
    dir
}

#[test]
fn extension_rules_match_svn_path_splitext() {
    assert_eq!(path_extension("a/b.txt"), "txt");
    assert_eq!(path_extension("a/b.tar.gz"), "gz");
    assert_eq!(path_extension("a/.profile"), "");
    assert_eq!(path_extension(".profile"), "");
    assert_eq!(path_extension("a.b/c"), "");
    assert_eq!(path_extension("file."), "");
    assert_eq!(path_extension("noext"), "");
}

#[test]
fn preserved_extension_follows_the_patterns() {
    let pats = vec![
        "c".to_string(),
        "t*".to_string(),
        "[hH]pp".to_string(),
        "?s".to_string(),
    ];
    assert_eq!(preserved_extension("src/a.c", &pats), "c");
    assert_eq!(preserved_extension("a.txt", &pats), "txt");
    assert_eq!(preserved_extension("a.Hpp", &pats), "Hpp");
    assert_eq!(preserved_extension("a.js", &pats), "js");
    // apr_fnmatch: a backslash makes the next character literal.
    let esc = vec![r"tx\t".to_string(), r"[\]x]y".to_string()];
    assert_eq!(preserved_extension("a.txt", &esc), "txt");
    assert_eq!(preserved_extension("a.tyt", &esc), "");
    assert_eq!(preserved_extension("a.]y", &esc), "]y");
    let literal_star = vec![r"\*".to_string()];
    assert_eq!(preserved_extension("a.*", &literal_star), "*");
    assert_eq!(preserved_extension("a.b", &literal_star), "");
    assert_eq!(preserved_extension("a.rs.bak", &pats), "");
    assert_eq!(preserved_extension("a.h", &pats), "");
    assert_eq!(preserved_extension("a.c", &[]), "");
    assert_eq!(preserved_extension("Makefile", &pats), "");
}

#[test]
fn labels_match_svn_for_update_and_merge() {
    let u = TextMarkerLabels::for_update(Some(1), 2, "");
    assert_eq!(
        (u.original.as_str(), u.mine.as_str(), u.their.as_str()),
        (".r1", ".mine", ".r2")
    );
    let u = TextMarkerLabels::for_update(None, 5, "txt");
    assert_eq!(
        (u.original.as_str(), u.mine.as_str(), u.their.as_str()),
        (".r0.txt", ".mine.txt", ".r5.txt")
    );
    let u = TextMarkerLabels::for_update(Some(-1), 5, "");
    assert_eq!(u.original, ".r0");
    let m = TextMarkerLabels::for_merge(2, 3, "c");
    assert_eq!(
        (m.original.as_str(), m.mine.as_str(), m.their.as_str()),
        (".merge-left.r2.c", ".working.c", ".merge-right.r3.c")
    );
}

#[test]
fn unique_names_count_up_like_svn() {
    let dir = tempfile::tempdir().unwrap();
    let names: Vec<_> = (0..4)
        .map(|_| {
            create_unique_file(dir.path(), "f.txt", ".mine")
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        names,
        ["f.txt.mine", "f.txt.2.mine", "f.txt.3.mine", "f.txt.4.mine"]
    );
}

#[cfg(unix)]
#[test]
fn an_existing_symlink_is_never_reused_or_followed() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let victim = outside.path().join("victim");
    fs::write(&victim, b"keep").unwrap();
    std::os::unix::fs::symlink(&victim, dir.path().join("f.prej")).unwrap();
    let made = create_unique_file(dir.path(), "f", ".prej").unwrap();
    assert_eq!(made.file_name().unwrap(), "f.2.prej");
    assert_eq!(fs::read(&victim).unwrap(), b"keep");
}

fn plain() -> TranslateInfo {
    TranslateInfo::default()
}

fn write_wc_file(w: &tempfile::TempDir, rel: &str, bytes: &[u8]) {
    let p = w.path().join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

#[test]
fn text_markers_for_an_update_conflict() {
    let w = wc();
    write_wc_file(&w, "trunk/t.txt", b"mine\r\nline\r\n");
    let labels = TextMarkerLabels::for_update(Some(1), 2, "");
    let info = TranslateInfo {
        eol_style: crate::translate::EolStyle::CrLf,
        keywords_prop: None,
        special: false,
    };
    let m = create_text_markers(
        w.path(),
        "trunk/t.txt",
        &labels,
        &MarkerSource::InWc(".svn/tmp/left".into()),
        &MarkerSource::InWc(".svn/tmp/right".into()),
        &info,
    )
    .unwrap();
    assert_eq!(m.conflict.original.as_deref(), Some("trunk/t.txt.r1"));
    assert_eq!(m.conflict.mine.as_deref(), Some("trunk/t.txt.mine"));
    assert_eq!(m.conflict.their.as_deref(), Some("trunk/t.txt.r2"));
    for f in ["t.txt.r1", "t.txt.mine", "t.txt.r2"] {
        assert_eq!(
            fs::metadata(w.path().join("trunk").join(f)).unwrap().len(),
            0,
            "{f}"
        );
    }
    let [
        WorkItem::FileTranslate {
            src: s1, dst: d1, ..
        },
        WorkItem::FileTranslate {
            src: s2, dst: d2, ..
        },
        WorkItem::FileTranslate {
            src: s3,
            dst: d3,
            local,
        },
        WorkItem::FileRemove { relpath: removed },
    ] = &m.work_items[..]
    else {
        panic!("{:?}", m.work_items)
    };
    assert_eq!(
        (s1.as_str(), d1.as_str()),
        (".svn/tmp/left", "trunk/t.txt.r1")
    );
    assert_eq!(
        (s2.as_str(), d2.as_str()),
        (".svn/tmp/right", "trunk/t.txt.r2")
    );
    assert_eq!(
        (d3.as_str(), local.as_str()),
        ("trunk/t.txt.mine", "trunk/t.txt")
    );
    // The third source and the removal are the freshly staged detranslated copy.
    assert!(s3.starts_with(".svn/tmp/marker-"), "{s3}");
    assert_eq!(removed, s3);
    assert_eq!(
        fs::read(w.path().join(s3)).unwrap(),
        b"mine\r\nline\r\n",
        "CRLF repo-normal form is CRLF"
    );
    // A second conflict on the same file gets numbered names.
    let again = create_text_markers(
        w.path(),
        "trunk/t.txt",
        &labels,
        &MarkerSource::InWc("l".into()),
        &MarkerSource::InWc("r".into()),
        &plain(),
    )
    .unwrap();
    assert_eq!(again.conflict.mine.as_deref(), Some("trunk/t.txt.2.mine"));
}

#[test]
fn detranslation_normalises_the_working_file() {
    let w = wc();
    write_wc_file(&w, "f", b"$Id: f 7 2026-01-01 00:00:00Z al $\nx\r\ny\r\n");
    let info = TranslateInfo {
        eol_style: crate::translate::EolStyle::Native,
        keywords_prop: Some(b"Id".to_vec()),
        special: false,
    };
    let m = create_text_markers(
        w.path(),
        "f",
        &TextMarkerLabels::for_update(Some(1), 2, ""),
        &MarkerSource::InWc("a".into()),
        &MarkerSource::InWc("b".into()),
        &info,
    )
    .unwrap();
    let WorkItem::FileTranslate { src, .. } = &m.work_items[2] else {
        panic!()
    };
    // The working file mixes endings; detranslation repairs them (force repair) to LF.
    assert_eq!(fs::read(w.path().join(src)).unwrap(), b"$Id$\nx\ny\n");
}

#[cfg(unix)]
#[test]
fn a_symlink_working_file_is_staged_as_special_text() {
    let w = wc();
    std::os::unix::fs::symlink("some/dest", w.path().join("l")).unwrap();
    let m = create_text_markers(
        w.path(),
        "l",
        &TextMarkerLabels::for_update(Some(1), 2, ""),
        &MarkerSource::InWc("a".into()),
        &MarkerSource::InWc("b".into()),
        &plain(),
    )
    .unwrap();
    let WorkItem::FileTranslate { src, .. } = &m.work_items[2] else {
        panic!()
    };
    assert_eq!(fs::read(w.path().join(src)).unwrap(), b"link some/dest");
}

/// The class of bug the public API used to allow: a caller handing in "the working file" as the
/// thing to delete. No input can produce a `file-remove` of anything but a staged `.svn/tmp` file.
#[test]
fn queueing_can_never_remove_the_working_file_or_any_wc_path() {
    let w = wc();
    write_wc_file(&w, "t.txt", b"data");
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("ext"), b"external").unwrap();
    let sources = [
        (
            MarkerSource::InWc("t.txt".into()),
            MarkerSource::InWc("t.txt".into()),
        ),
        (
            MarkerSource::InWc(".svn/tmp/x".into()),
            MarkerSource::External(outside.path().join("ext")),
        ),
        (
            MarkerSource::External(outside.path().join("ext")),
            MarkerSource::External(outside.path().join("ext")),
        ),
    ];
    for (original, their) in sources {
        let m = create_text_markers(
            w.path(),
            "t.txt",
            &TextMarkerLabels::for_merge(1, 2, ""),
            &original,
            &their,
            &plain(),
        )
        .unwrap();
        let removals: Vec<_> = m
            .work_items
            .iter()
            .filter_map(|i| {
                if let WorkItem::FileRemove { relpath } = i {
                    Some(relpath.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert!(!removals.is_empty());
        for r in &removals {
            assert!(r.starts_with(".svn/tmp/marker-"), "unexpected removal {r}");
            assert_ne!(*r, "t.txt");
        }
        assert_eq!(fs::read(w.path().join("t.txt")).unwrap(), b"data");
        // Sources are never removal targets.
        let sources_of_translate: Vec<_> = m
            .work_items
            .iter()
            .filter_map(|i| {
                if let WorkItem::FileTranslate { src, .. } = i {
                    Some(src.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(sources_of_translate.len(), 3);
        // The staged detranslated copy is removed first, then staged external copies.
        let WorkItem::FileRemove { relpath: first } = &m.work_items[3] else {
            panic!()
        };
        assert_eq!(first, sources_of_translate[2]);
    }
}

#[test]
fn in_wc_sources_must_be_paths_the_runner_accepts() {
    let w = wc();
    write_wc_file(&w, "t.txt", b"data");
    for bad in [
        "../x",
        "/abs",
        ".svn/wc.db",
        ".svn/tmp/a/b",
        ".svn/pristine/zz/zz.svn-base",
        ".svn/pristine",
    ] {
        let r = create_text_markers(
            w.path(),
            "t.txt",
            &TextMarkerLabels::for_update(Some(1), 2, ""),
            &MarkerSource::InWc(bad.into()),
            &MarkerSource::InWc("ok".into()),
            &plain(),
        );
        assert!(r.is_err(), "{bad}");
        // Nothing was created before the validation failed.
        assert!(!w.path().join("t.txt.r1").exists(), "{bad}");
    }
}

#[test]
fn external_sources_are_staged_inside_the_working_copy() {
    let w = wc();
    write_wc_file(&w, "f", b"data");
    let outside = tempfile::tempdir().unwrap();
    let (l, r) = (outside.path().join("l"), outside.path().join("r"));
    fs::write(&l, b"left").unwrap();
    fs::write(&r, b"right").unwrap();
    let m = create_text_markers(
        w.path(),
        "f",
        &TextMarkerLabels::for_merge(2, 3, ""),
        &MarkerSource::External(l),
        &MarkerSource::External(r),
        &plain(),
    )
    .unwrap();
    let WorkItem::FileTranslate { src, .. } = &m.work_items[0] else {
        panic!("{:?}", m.work_items)
    };
    assert!(src.starts_with(".svn/tmp/marker-"), "{src}");
    assert_eq!(fs::read(w.path().join(src)).unwrap(), b"left");
    // translate x3, remove detranslated copy, then the two staged external copies (svn order).
    assert_eq!(m.work_items.len(), 6);
    assert!(
        m.work_items[3..]
            .iter()
            .all(|i| matches!(i, WorkItem::FileRemove { .. }))
    );
    assert_eq!(m.conflict.original.as_deref(), Some("f.merge-left.r2"));
}

#[test]
fn prop_markers_for_files_and_directories() {
    let w = wc();
    fs::create_dir_all(w.path().join("d/sub")).unwrap();
    let f = create_prop_marker(w.path(), "d/a.txt", false).unwrap();
    assert_eq!(f.relpath, "d/a.txt.prej");
    assert_eq!(
        f.work_item,
        WorkItem::PrejInstall {
            relpath: "d/a.txt".into(),
            legacy_conflict: None
        }
    );
    assert!(w.path().join("d/a.txt.prej").is_file());
    let f2 = create_prop_marker(w.path(), "d/a.txt", false).unwrap();
    assert_eq!(f2.relpath, "d/a.txt.2.prej");
    let d = create_prop_marker(w.path(), "d/sub", true).unwrap();
    assert_eq!(d.relpath, "d/sub/dir_conflicts.prej");
    assert!(w.path().join("d/sub/dir_conflicts.prej").is_file());
    // The root directory itself.
    let root = create_prop_marker(w.path(), "", true).unwrap();
    assert_eq!(root.relpath, "dir_conflicts.prej");

    let mut p = super::super::PropConflict::default();
    p.add_marker(f.relpath.clone());
    p.add_marker(f2.relpath);
    assert_eq!(p.markers, ["d/a.txt.2.prej", "d/a.txt.prej"]);
}

#[test]
fn unsafe_paths_are_rejected() {
    let w = wc();
    for bad in ["../x", "/abs/x", "a/../../x"] {
        assert!(create_prop_marker(w.path(), bad, false).is_err(), "{bad}");
    }
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), w.path().join("link")).unwrap();
        assert!(create_prop_marker(w.path(), "link/f", false).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}

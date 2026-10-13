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

#[test]
fn text_markers_for_an_update_conflict() {
    let w = wc();
    fs::create_dir_all(w.path().join("trunk")).unwrap();
    let labels = TextMarkerLabels::for_update(Some(1), 2, "");
    let m = create_text_markers(
        w.path(),
        "trunk/t.txt",
        &labels,
        &MarkerSource::InWc(".svn/tmp/left".into()),
        &MarkerSource::InWc(".svn/tmp/right".into()),
        &MarkerSource::InWc(".svn/tmp/target".into()),
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
    assert_eq!(
        m.work_items,
        vec![
            WorkItem::FileTranslate {
                local: "trunk/t.txt".into(),
                src: ".svn/tmp/left".into(),
                dst: "trunk/t.txt.r1".into()
            },
            WorkItem::FileTranslate {
                local: "trunk/t.txt".into(),
                src: ".svn/tmp/right".into(),
                dst: "trunk/t.txt.r2".into()
            },
            WorkItem::FileTranslate {
                local: "trunk/t.txt".into(),
                src: ".svn/tmp/target".into(),
                dst: "trunk/t.txt.mine".into()
            },
            WorkItem::FileRemove {
                relpath: ".svn/tmp/target".into()
            },
        ]
    );
    // A second conflict on the same file gets numbered names.
    let again = create_text_markers(
        w.path(),
        "trunk/t.txt",
        &labels,
        &MarkerSource::InWc("l".into()),
        &MarkerSource::InWc("r".into()),
        &MarkerSource::InWc("t".into()),
    )
    .unwrap();
    assert_eq!(again.conflict.mine.as_deref(), Some("trunk/t.txt.2.mine"));
}

#[test]
fn external_sources_are_staged_inside_the_working_copy() {
    let w = wc();
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
        &MarkerSource::InWc("t".into()),
    )
    .unwrap();
    let WorkItem::FileTranslate { src, .. } = &m.work_items[0] else {
        panic!("{:?}", m.work_items)
    };
    assert!(src.starts_with(".svn/tmp/marker-"), "{src}");
    assert_eq!(fs::read(w.path().join(src)).unwrap(), b"left");
    let removals: Vec<_> = m
        .work_items
        .iter()
        .filter(|i| matches!(i, WorkItem::FileRemove { .. }))
        .collect();
    assert_eq!(removals.len(), 3, "target copy plus the two staged files");
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

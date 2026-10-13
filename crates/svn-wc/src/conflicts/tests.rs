//! Conflict skel tests; fixtures are `conflict_data` blobs written by svn 1.15.0
//! (`tests/compat/scenarios/wc/conflicts/generate.py`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use super::*;

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/compat/scenarios/wc/conflicts")
        .join(format!("{name}.skel"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn svn_written_skels_roundtrip_byte_for_byte() {
    for name in [
        "update_t_txt",
        "update_d_txt",
        "merge_m_txt",
        "update2_mv_txt",
        "update2_p_txt",
        "switch_s_txt",
    ] {
        let bytes = fixture(name);
        let parsed = ConflictSkel::decode(&bytes).unwrap();
        assert!(parsed.is_complete(), "{name}");
        assert_eq!(parsed.encode(), bytes, "{name}");
    }
}

#[test]
fn update_text_and_prop_conflict_fields() {
    let c = ConflictSkel::decode(&fixture("update_t_txt")).unwrap();
    let op = c.operation.as_ref().unwrap();
    assert_eq!(op.operation, Operation::Update);
    let (Origin::Subversion(left), Origin::Subversion(right)) = (&op.left, &op.right) else {
        panic!("expected repository origins");
    };
    assert_eq!(
        (
            left.repos_relpath.as_str(),
            left.peg_rev,
            left.node_kind.as_str()
        ),
        ("t.txt", 1, "file")
    );
    assert_eq!((right.peg_rev, right.node_kind.as_str()), (2, "file"));
    assert!(left.repos_uuid.is_some());

    let text = c.text().unwrap();
    assert_eq!(text.original.as_deref(), Some("t.txt.r1"));
    assert_eq!(text.mine.as_deref(), Some("t.txt.mine"));
    assert_eq!(text.their.as_deref(), Some("t.txt.r2"));

    let prop = c.prop().unwrap();
    assert_eq!(prop.markers, ["t.txt.prej"]);
    assert_eq!(prop.names, ["color"]);
    let value = |p: &Option<Vec<PropPair>>| p.as_ref().unwrap()[0].1.clone();
    assert_eq!(value(&prop.old_props), b"red");
    assert_eq!(value(&prop.mine_props), b"green");
    assert_eq!(value(&prop.their_props), b"blue");
    assert!(c.tree().is_none());
}

#[test]
fn tree_conflict_fields() {
    let c = ConflictSkel::decode(&fixture("update_d_txt")).unwrap();
    let t = c.tree().unwrap();
    assert_eq!(
        (&t.reason, &t.action),
        (&TreeReason::Edited, &TreeAction::Delete)
    );
    assert_eq!((&t.move_src_op_root, &t.move_dst_op_root), (&None, &None));
    let Origin::Subversion(right) = &c.operation.as_ref().unwrap().right else {
        panic!()
    };
    assert_eq!(right.node_kind, "none");
}

#[test]
fn merge_conflict_markers() {
    let c = ConflictSkel::decode(&fixture("merge_m_txt")).unwrap();
    assert_eq!(c.operation.as_ref().unwrap().operation, Operation::Merge);
    let t = c.text().unwrap();
    assert_eq!(t.original.as_deref(), Some("m.txt.merge-left.r2"));
    assert_eq!(t.mine.as_deref(), Some("m.txt.working"));
    assert_eq!(t.their.as_deref(), Some("m.txt.merge-right.r3"));
}

/// Builds a skel the way `svn_wc__conflict_skel_*` do and compares with svn's own bytes.
#[test]
fn builder_matches_what_svn_wrote() {
    let bytes = fixture("update_t_txt");
    let reference = ConflictSkel::decode(&bytes).unwrap();
    let op = reference.operation.clone().unwrap();
    let mut built = ConflictSkel {
        operation: Some(op),
        conflicts: Vec::new(),
    };
    let Conflict::Prop(p) = reference.conflicts[1].clone() else {
        panic!("fixture layout changed")
    };
    // Each add prepends: the property conflict went in first, the text conflict second.
    built.add(Conflict::Prop(p)).unwrap();
    built
        .add(Conflict::Text(reference.text().unwrap().clone()))
        .unwrap();
    assert_eq!(built.encode(), bytes);
}

#[test]
fn tree_conflict_with_move_roots_and_unset_origins() {
    let tree = TreeConflict {
        reason: TreeReason::MovedAway,
        action: TreeAction::Edit,
        move_src_op_root: Some("a".into()),
        move_dst_op_root: Some("b/c".into()),
    };
    let skel = ConflictSkel {
        operation: Some(OperationInfo {
            operation: Operation::Switch,
            left: Origin::Unset,
            right: Origin::Subversion(Location {
                repos_root_url: "http://h/r".into(),
                repos_uuid: None,
                repos_relpath: "trunk/x y".into(),
                peg_rev: 12,
                node_kind: "dir".into(),
            }),
        }),
        conflicts: vec![Conflict::Tree(tree)],
    };
    let bytes = skel.encode();
    assert_eq!(
        String::from_utf8(bytes.clone()).unwrap(),
        "((switch (() (subversion http://h/r () 9 trunk/x y 2 12 dir))) ((tree () moved-away edited a b/c)))"
    );
    assert_eq!(ConflictSkel::decode(&bytes).unwrap(), skel);
}

#[test]
fn unknown_parts_survive_a_roundtrip() {
    let raw = b"((frobnicate (() (other thing))) ((reject (x y)) (tree () weird odd)))";
    let c = ConflictSkel::decode(raw).unwrap();
    assert_eq!(
        c.operation.as_ref().unwrap().operation,
        Operation::Other("frobnicate".into())
    );
    assert_eq!(
        c.operation.as_ref().unwrap().right,
        Origin::Other(Skel::list(vec![Skel::atom("other"), Skel::atom("thing")]))
    );
    assert!(matches!(c.conflicts[0], Conflict::Other(_)));
    assert_eq!(c.tree().unwrap().reason, TreeReason::Other("weird".into()));
    assert_eq!(c.encode(), raw);
}

#[test]
fn incomplete_and_resolve() {
    let mut c = ConflictSkel::default();
    assert!(!c.is_complete());
    assert_eq!(c.encode(), b"(() ())");
    assert!(
        c.resolve(true, None, true).is_err(),
        "resolve needs the operation"
    );

    let mut c = ConflictSkel::decode(&fixture("update_t_txt")).unwrap();
    assert!(
        !c.resolve(true, None, false).unwrap(),
        "prop conflict is still there"
    );
    assert!(c.text().is_none() && c.prop().is_some());
    assert!(
        !c.resolve(false, Some("other"), false).unwrap(),
        "unknown property name resolves nothing"
    );
    assert!(c.resolve(false, Some("color"), false).unwrap());
    assert!(c.conflicts.is_empty());

    let mut c = ConflictSkel::decode(&fixture("update_t_txt")).unwrap();
    assert!(
        !c.resolve(false, Some(""), false).unwrap(),
        "all props resolved, text remains"
    );
    assert!(c.prop().is_none() && c.text().is_some());
}

#[test]
fn adding_a_second_conflict_of_the_same_kind_is_refused() {
    let mut c = ConflictSkel::decode(&fixture("update_d_txt")).unwrap();
    let again = Conflict::Tree(c.tree().unwrap().clone());
    assert!(c.add(again).is_err());
}

#[test]
fn malformed_skels_are_rejected() {
    for bad in [
        &b"()"[..],
        b"(() () ())",
        b"update",
        b"((update) ())",
        b"((update (() () ())) ())",
        b"((update (() ())) (text))",
        b"((update (() ())) ((text (a b))))",
        b"((update (() ())) ((prop (a) (b) c d)))",
        b"((update (() ())) ((tree () edited)))",
        b"((update ((subversion r () p x file) ())) ())",
        b"((update ((subversion r))) ())",
    ] {
        assert!(
            ConflictSkel::decode(bad).is_err(),
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
}

#[test]
fn moved_away_tree_conflict_carries_the_move_roots() {
    let c = ConflictSkel::decode(&fixture("update2_mv_txt")).unwrap();
    let t = c.tree().unwrap();
    assert_eq!(
        (&t.reason, &t.action),
        (&TreeReason::MovedAway, &TreeAction::Edit)
    );
    assert_eq!(t.move_src_op_root.as_deref(), Some("mv.txt"));
    assert_eq!(t.move_dst_op_root.as_deref(), Some("moved.txt"));
}

#[test]
fn prop_conflict_with_several_names_and_a_property_missing_on_one_side() {
    let c = ConflictSkel::decode(&fixture("update2_p_txt")).unwrap();
    let p = c.prop().unwrap();
    let mut names = p.names.clone();
    names.sort();
    assert_eq!(names, ["added", "one", "two"]);
    let has = |props: &Option<Vec<PropPair>>, name: &str| {
        props
            .as_ref()
            .unwrap()
            .iter()
            .any(|(n, _)| n == name.as_bytes())
    };
    // `added` did not exist in the common ancestor but was added on both sides.
    assert!(!has(&p.old_props, "added"));
    assert!(has(&p.mine_props, "added") && has(&p.their_props, "added"));
    assert!(has(&p.old_props, "one") && has(&p.old_props, "two"));
}

#[test]
fn switch_conflict_origins_name_both_branches() {
    let c = ConflictSkel::decode(&fixture("switch_s_txt")).unwrap();
    let op = c.operation.as_ref().unwrap();
    assert_eq!(op.operation, Operation::Switch);
    let (Origin::Subversion(from), Origin::Subversion(to)) = (&op.left, &op.right) else {
        panic!()
    };
    assert_eq!(
        (from.repos_relpath.as_str(), to.repos_relpath.as_str()),
        ("s.txt", "br/s.txt")
    );
    // Numbered marker names show the unique-name rule in the wild.
    let t = c.text().unwrap();
    assert_eq!(t.their.as_deref(), Some("s.txt.2.r10"));
}

#[test]
fn absent_property_sides_are_recorded_as_empty_atoms() {
    let p = PropConflict {
        markers: vec![],
        names: vec!["k".into()],
        old_props: None,
        mine_props: Some(vec![(b"k".to_vec(), b"v".to_vec())]),
        their_props: None,
    };
    let skel = ConflictSkel {
        operation: Some(OperationInfo {
            operation: Operation::Update,
            left: Origin::Unset,
            right: Origin::Unset,
        }),
        conflicts: vec![Conflict::Prop(p.clone())],
    };
    let bytes = skel.encode();
    assert_eq!(
        String::from_utf8(bytes.clone()).unwrap(),
        "((update (() ())) ((prop () (k) 0  (k v) 0 )))"
    );
    assert_eq!(ConflictSkel::decode(&bytes).unwrap().prop().unwrap(), &p);
}

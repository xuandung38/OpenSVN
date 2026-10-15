//! Per-folder depths of the fake working copy: which repository paths a working copy has after
//! a checkout at one depth followed by `svn update --parents --set-depth` on some folders, and
//! the depth each folder records. Pure functions; the real engine leaves this to `svn-client`.

use std::collections::{BTreeMap, BTreeSet};

use opensvn_ipc::{FileKind, SparseDepth};
use svn_types::RelPath;

/// What a working copy has: every present path (folders with their recorded depth, files with
/// `None`) and the excluded folders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Layout {
    pub present: BTreeMap<RelPath, Option<SparseDepth>>,
    pub excluded: BTreeSet<RelPath>,
}

/// The repository tree the layout is taken from: path → kind, the root included.
pub(super) type Tree = BTreeMap<RelPath, FileKind>;

fn children<'a>(tree: &'a Tree, dir: &'a RelPath) -> impl Iterator<Item = (&'a RelPath, FileKind)> {
    tree.iter()
        .filter(move |(p, _)| !p.is_root() && p.parent().as_ref() == Some(dir))
        .map(|(p, k)| (p, *k))
}

impl Layout {
    /// A checkout of `tree` at `depth`.
    pub(super) fn checkout(tree: &Tree, depth: SparseDepth) -> Self {
        let mut l = Self::default();
        l.present.insert(RelPath::root(), Some(depth));
        l.fill(tree, &RelPath::root(), depth);
        l
    }

    /// Adds what `depth` covers below the present folder `dir`.
    fn fill(&mut self, tree: &Tree, dir: &RelPath, depth: SparseDepth) {
        for (child, kind) in children(tree, dir) {
            match (depth, kind) {
                (SparseDepth::Empty | SparseDepth::Exclude, _) => {}
                (SparseDepth::Files, FileKind::Dir) => {}
                (_, FileKind::Dir) if depth == SparseDepth::Immediates => {
                    self.excluded.remove(child);
                    self.present
                        .entry(child.clone())
                        .or_insert(Some(SparseDepth::Empty));
                }
                (_, FileKind::Dir) => {
                    // Infinity: every folder below becomes infinite, as with svn.
                    self.excluded.remove(child);
                    self.present
                        .insert(child.clone(), Some(SparseDepth::Infinity));
                    self.fill(tree, child, SparseDepth::Infinity);
                }
                _ => {
                    self.present.insert(child.clone(), None);
                }
            }
        }
    }

    /// `svn update --parents --set-depth depth path`. `false` when `path` is not a folder of
    /// `tree` (the step changes nothing).
    pub(super) fn set_depth(&mut self, tree: &Tree, path: &RelPath, depth: SparseDepth) -> bool {
        if tree.get(path) != Some(&FileKind::Dir) {
            return false;
        }
        if depth == SparseDepth::Exclude {
            if path.is_root() || !self.present.contains_key(path) {
                return false;
            }
            self.present.retain(|p, _| !p.starts_with(path));
            self.excluded.retain(|p| !p.starts_with(path));
            self.excluded.insert(path.clone());
            return true;
        }
        // Missing folders above come in at depth empty.
        let mut ancestors = Vec::new();
        let mut cur = path.parent();
        while let Some(p) = cur.filter(|p| !p.is_root()) {
            cur = p.parent();
            ancestors.push(p);
        }
        for a in ancestors.into_iter().rev() {
            if !self.present.contains_key(&a) {
                self.excluded.remove(&a);
                self.present.insert(a, Some(SparseDepth::Empty));
            }
        }
        // Like svn 1.15, the folder ends up as a checkout at `depth` would leave it: what a
        // folder brought in below it had is cropped (`immediates`) or dropped (`files`, `empty`).
        self.present
            .retain(|p, _| p == path || !p.starts_with(path));
        self.excluded.retain(|p| !p.starts_with(path));
        self.present.insert(path.clone(), Some(depth));
        self.fill(tree, path, depth);
        true
    }

    /// The recorded depth of every folder below the root, parents first.
    pub(super) fn depths(&self) -> Vec<(RelPath, SparseDepth)> {
        let mut out: Vec<(RelPath, SparseDepth)> = self
            .present
            .iter()
            .filter(|(p, _)| !p.is_root())
            .filter_map(|(p, d)| d.map(|d| (p.clone(), d)))
            .chain(
                self.excluded
                    .iter()
                    .map(|p| (p.clone(), SparseDepth::Exclude)),
            )
            .collect();
        out.sort_by_cached_key(|(p, _)| p.components().map(str::to_owned).collect::<Vec<_>>());
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn tree() -> Tree {
        let mut t = Tree::new();
        for (p, k) in [
            ("", FileKind::Dir),
            ("a", FileKind::Dir),
            ("a/f", FileKind::File),
            ("a/b", FileKind::Dir),
            ("a/b/g", FileKind::File),
            ("c", FileKind::Dir),
            ("top", FileKind::File),
        ] {
            t.insert(RelPath::new(p).unwrap(), k);
        }
        t
    }

    fn paths(l: &Layout) -> Vec<&str> {
        l.present.keys().map(RelPath::as_str).collect()
    }

    #[test]
    fn steps_follow_svn_set_depth() {
        let t = tree();
        let mut l = Layout::checkout(&t, SparseDepth::Empty);
        assert_eq!(paths(&l), [""]);
        assert!(l.set_depth(&t, &RelPath::new("a/b").unwrap(), SparseDepth::Infinity));
        assert_eq!(paths(&l), ["", "a", "a/b", "a/b/g"]);
        assert_eq!(
            l.present[&RelPath::new("a").unwrap()],
            Some(SparseDepth::Empty)
        );
        // As in svn 1.15: `files` drops the folder brought in below.
        assert!(l.set_depth(&t, &RelPath::new("a").unwrap(), SparseDepth::Files));
        assert_eq!(paths(&l), ["", "a", "a/f"]);
        let mut full = Layout::checkout(&t, SparseDepth::Infinity);
        assert!(full.set_depth(&t, &RelPath::new("a").unwrap(), SparseDepth::Exclude));
        assert_eq!(paths(&full), ["", "c", "top"]);
        assert_eq!(
            full.depths(),
            [
                (RelPath::new("a").unwrap(), SparseDepth::Exclude),
                (RelPath::new("c").unwrap(), SparseDepth::Infinity)
            ]
        );
        assert!(full.set_depth(&t, &RelPath::root(), SparseDepth::Immediates));
        assert_eq!(paths(&full), ["", "a", "c", "top"]);
        assert!(!full.set_depth(&t, &RelPath::new("top").unwrap(), SparseDepth::Empty));
    }
}

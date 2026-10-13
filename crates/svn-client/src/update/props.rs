//! Three-way property merge for incoming `change_*_prop` calls.

use std::collections::BTreeMap;

use svn_types::PropChange;
use svn_wc::db::Props;

/// Result of merging incoming property changes into the local overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropMerge {
    /// Pristine properties after the update.
    pub new_pristine: Props,
    /// Working properties after the update (local changes preserved).
    pub new_actual: Props,
    /// Names whose incoming change collided with a different local change.
    pub conflicted: Vec<String>,
    /// At least one incoming change was applied to the working properties.
    pub applied: bool,
}

/// True for the bookkeeping properties the RA layer sends alongside real ones; they never reach
/// the property tables (`svn:entry:*` feeds the node row, `svn:wc:*` and `svn:dav:*` are caches).
pub fn is_special_prop(name: &str) -> bool {
    name.starts_with("svn:entry:") || name.starts_with("svn:wc:") || name.starts_with("svn:dav:")
}

fn to_map(p: &Props) -> BTreeMap<Vec<u8>, Vec<u8>> {
    p.iter().cloned().collect()
}

fn to_props(m: BTreeMap<Vec<u8>, Vec<u8>>) -> Props {
    m.into_iter().collect()
}

/// Applies `changes` (relative to `old_pristine`) to the pristine set and merges them into the
/// working set `actual`. A change collides when the local value differs from both the old
/// pristine value and the incoming one; the local value is kept and the name reported.
pub fn merge_props(old_pristine: &Props, changes: &[PropChange], actual: &Props) -> PropMerge {
    let mut pristine = to_map(old_pristine);
    let old = to_map(old_pristine);
    let mut mine = to_map(actual);
    let mut conflicted = Vec::new();
    let mut applied = false;
    for c in changes.iter().filter(|c| !is_special_prop(&c.name)) {
        let key = c.name.clone().into_bytes();
        let incoming = c.value.as_ref().map(|v| v.to_vec());
        let before = old.get(&key).cloned();
        let local = mine.get(&key).cloned();
        match &incoming {
            Some(v) => {
                pristine.insert(key.clone(), v.clone());
            }
            None => {
                pristine.remove(&key);
            }
        }
        if local == before {
            match incoming {
                Some(v) => {
                    mine.insert(key, v);
                }
                None => {
                    mine.remove(&key);
                }
            }
            applied = true;
        } else if local != incoming {
            conflicted.push(c.name.clone());
        }
    }
    PropMerge {
        new_pristine: to_props(pristine),
        new_actual: to_props(mine),
        conflicted,
        applied,
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn p(pairs: &[(&str, &str)]) -> Props {
        pairs
            .iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect()
    }

    fn set(name: &str, v: &str) -> PropChange {
        PropChange {
            name: name.into(),
            value: Some(Bytes::from(v.to_owned())),
        }
    }

    #[test]
    fn untouched_property_takes_incoming_value() {
        let m = merge_props(&p(&[("a", "1")]), &[set("a", "2")], &p(&[("a", "1")]));
        assert_eq!(m.new_pristine, p(&[("a", "2")]));
        assert_eq!(m.new_actual, p(&[("a", "2")]));
        assert!(m.conflicted.is_empty());
    }

    #[test]
    fn local_change_survives_unrelated_incoming_change() {
        let m = merge_props(&p(&[("a", "1")]), &[set("b", "x")], &p(&[("a", "9")]));
        assert_eq!(m.new_actual, p(&[("a", "9"), ("b", "x")]));
        assert!(m.conflicted.is_empty());
    }

    #[test]
    fn same_value_on_both_sides_merges() {
        let m = merge_props(&p(&[("a", "1")]), &[set("a", "2")], &p(&[("a", "2")]));
        assert!(m.conflicted.is_empty());
        assert_eq!(m.new_actual, m.new_pristine);
    }

    #[test]
    fn different_local_value_conflicts_and_is_kept() {
        let m = merge_props(&p(&[("a", "1")]), &[set("a", "2")], &p(&[("a", "3")]));
        assert_eq!(m.conflicted, vec!["a".to_owned()]);
        assert_eq!(m.new_actual, p(&[("a", "3")]));
        assert_eq!(m.new_pristine, p(&[("a", "2")]));
    }

    #[test]
    fn bookkeeping_properties_are_ignored() {
        let m = merge_props(&p(&[]), &[set("svn:entry:committed-rev", "4")], &p(&[]));
        assert!(m.new_pristine.is_empty() && !m.applied);
    }
}

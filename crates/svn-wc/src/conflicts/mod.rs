//! Conflict skel codec (track B), the format stored in `ACTUAL_NODE.conflict_data`.
//!
//! Grammar and positional order verified against `libsvn_wc/conflicts.c` at tags 1.14.5 and
//! 1.15.0 (identical):
//!
//! ```text
//! conflict  := (WHY CONFLICTS)
//! WHY       := ()                        ; operation not set yet
//!            | (OP (LEFT RIGHT))         ; update/switch: (original target); merge: (left right)
//! OP        := "update" | "switch" | "merge" | "patch"
//! LEFT, RIGHT := () | ("subversion" repos_root_url (uuid | ()) repos_relpath peg_rev kind)
//! CONFLICTS := (CONFLICT ...)            ; newest first; empty => the skel is incomplete
//! text      := ("text" (original_marker mine_marker their_marker))   ; marker := relpath | ()
//! prop      := ("prop" (marker_relpath ...) (conflicted_name ...) old mine their)
//!                       ; old/mine/their := proplist skel | "" (absent)
//! tree      := ("tree" () reason action [move_src_op_root_relpath [move_dst_op_root_relpath]])
//! reason    := edited obstructed deleted missing unversioned added replaced moved-away moved-here
//! action    := edited added deleted replaced
//! ```
//!
//! Unknown operations, origins and conflict kinds are kept verbatim so a skel written by a newer
//! svn survives a read-modify-write cycle. Marker file *creation* (`.mine`, `.rN`, `.prej`) is
//! separate and lives with the update/merge code that produces them.

pub mod markers;

use svn_types::{ErrorCode, Result, SvnError};

use crate::skel::{PropPair, Skel, parse_proplist, unparse_proplist};

const KIND_TEXT: &str = "text";
const KIND_PROP: &str = "prop";
const KIND_TREE: &str = "tree";
const SRC_SUBVERSION: &str = "subversion";

fn bad(what: impl Into<String>) -> SvnError {
    SvnError::new(
        ErrorCode::Corrupt,
        format!("malformed conflict skel: {}", what.into()),
    )
}

fn atom_string(s: &Skel, what: &str) -> Result<String> {
    let bytes = s
        .as_atom()
        .ok_or_else(|| bad(format!("{what} is not an atom")))?;
    String::from_utf8(bytes.to_vec()).map_err(|_| bad(format!("{what} is not UTF-8")))
}

fn list<'a>(s: &'a Skel, what: &str) -> Result<&'a [Skel]> {
    s.as_list()
        .ok_or_else(|| bad(format!("{what} is not a list")))
}

fn atom(text: &str) -> Skel {
    Skel::atom(text.as_bytes())
}

/// The operation that produced the conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// `svn update`.
    Update,
    /// `svn switch`.
    Switch,
    /// `svn merge`.
    Merge,
    /// `svn patch` (never written by update/switch/merge).
    Patch,
    /// A token this version does not know.
    Other(String),
}

impl Operation {
    fn token(&self) -> &str {
        match self {
            Self::Update => "update",
            Self::Switch => "switch",
            Self::Merge => "merge",
            Self::Patch => "patch",
            Self::Other(t) => t,
        }
    }

    fn from_token(t: String) -> Self {
        match t.as_str() {
            "update" => Self::Update,
            "switch" => Self::Switch,
            "merge" => Self::Merge,
            "patch" => Self::Patch,
            _ => Self::Other(t),
        }
    }
}

/// A repository location (`svn_wc_conflict_version_t`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// Repository root URL.
    pub repos_root_url: String,
    /// Repository UUID, if known.
    pub repos_uuid: Option<String>,
    /// Path in the repository, without leading slash.
    pub repos_relpath: String,
    /// Peg revision.
    pub peg_rev: i64,
    /// Node kind word as svn writes it (`file`, `dir`, `none`, `unknown`, `symlink`).
    pub node_kind: String,
}

/// One side of an operation's origin pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Not recorded (`()`).
    Unset,
    /// A Subversion repository location.
    Subversion(Location),
    /// Anything else, kept verbatim.
    Other(Skel),
}

impl Origin {
    fn parse(s: &Skel) -> Result<Self> {
        let items = list(s, "origin")?;
        let Some(first) = items.first() else {
            return Ok(Self::Unset);
        };
        if first.as_atom() != Some(SRC_SUBVERSION.as_bytes()) {
            return Ok(Self::Other(s.clone()));
        }
        let [_, root, uuid, relpath, rev, kind] = items else {
            return Err(bad("origin needs 6 elements"));
        };
        Ok(Self::Subversion(Location {
            repos_root_url: atom_string(root, "repository root")?,
            repos_uuid: match uuid {
                Skel::List(_) => None,
                a => Some(atom_string(a, "uuid")?),
            },
            repos_relpath: atom_string(relpath, "repository path")?,
            peg_rev: atom_string(rev, "revision")?
                .parse()
                .map_err(|_| bad("revision is not a number"))?,
            node_kind: atom_string(kind, "node kind")?,
        }))
    }

    fn to_skel(&self) -> Skel {
        match self {
            Self::Unset => Skel::list(vec![]),
            Self::Other(s) => s.clone(),
            Self::Subversion(l) => Skel::list(vec![
                atom(SRC_SUBVERSION),
                atom(&l.repos_root_url),
                l.repos_uuid
                    .as_deref()
                    .map_or_else(|| Skel::list(vec![]), atom),
                atom(&l.repos_relpath),
                atom(&l.peg_rev.to_string()),
                atom(&l.node_kind),
            ]),
        }
    }
}

/// The operation and its origin pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationInfo {
    /// Operation.
    pub operation: Operation,
    /// First origin: original (update/switch) or left (merge).
    pub left: Origin,
    /// Second origin: target (update/switch) or right (merge).
    pub right: Origin,
}

/// Text conflict: marker files relative to the working copy root.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextConflict {
    /// Common ancestor (`.rOLD`, `.merge-left.rN`).
    pub original: Option<String>,
    /// The user's version (`.mine`, `.working`).
    pub mine: Option<String>,
    /// The incoming version (`.rNEW`, `.merge-right.rN`).
    pub their: Option<String>,
}

/// Property conflict.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PropConflict {
    /// `.prej` reject files (svn writes at most one).
    pub markers: Vec<String>,
    /// Names of the conflicted properties.
    pub names: Vec<String>,
    /// Properties of the common ancestor; `None` when absent.
    pub old_props: Option<Vec<PropPair>>,
    /// The user's properties.
    pub mine_props: Option<Vec<PropPair>>,
    /// The incoming properties.
    pub their_props: Option<Vec<PropPair>>,
}

/// Why the local node is in the way of the incoming change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeReason {
    /// `edited`.
    Edited,
    /// `obstructed`.
    Obstructed,
    /// `deleted`.
    Deleted,
    /// `missing`.
    Missing,
    /// `unversioned`.
    Unversioned,
    /// `added`.
    Added,
    /// `replaced`.
    Replaced,
    /// `moved-away`.
    MovedAway,
    /// `moved-here`.
    MovedHere,
    /// Unknown token, kept verbatim (svn reads it as `edited`).
    Other(String),
}

/// What the incoming change tried to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeAction {
    /// `edited`.
    Edit,
    /// `added`.
    Add,
    /// `deleted`.
    Delete,
    /// `replaced`.
    Replace,
    /// Unknown token, kept verbatim (svn reads it as `edited`).
    Other(String),
}

impl TreeReason {
    fn token(&self) -> &str {
        match self {
            Self::Edited => "edited",
            Self::Obstructed => "obstructed",
            Self::Deleted => "deleted",
            Self::Missing => "missing",
            Self::Unversioned => "unversioned",
            Self::Added => "added",
            Self::Replaced => "replaced",
            Self::MovedAway => "moved-away",
            Self::MovedHere => "moved-here",
            Self::Other(t) => t,
        }
    }

    fn from_token(t: String) -> Self {
        match t.as_str() {
            "edited" => Self::Edited,
            "obstructed" => Self::Obstructed,
            "deleted" => Self::Deleted,
            "missing" => Self::Missing,
            "unversioned" => Self::Unversioned,
            "added" => Self::Added,
            "replaced" => Self::Replaced,
            "moved-away" => Self::MovedAway,
            "moved-here" => Self::MovedHere,
            _ => Self::Other(t),
        }
    }
}

impl TreeAction {
    fn token(&self) -> &str {
        match self {
            Self::Edit => "edited",
            Self::Add => "added",
            Self::Delete => "deleted",
            Self::Replace => "replaced",
            Self::Other(t) => t,
        }
    }

    fn from_token(t: String) -> Self {
        match t.as_str() {
            "edited" => Self::Edit,
            "added" => Self::Add,
            "deleted" => Self::Delete,
            "replaced" => Self::Replace,
            _ => Self::Other(t),
        }
    }
}

/// Tree conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeConflict {
    /// Local state.
    pub reason: TreeReason,
    /// Incoming change.
    pub action: TreeAction,
    /// Root of the local move (only with [`TreeReason::MovedAway`]).
    pub move_src_op_root: Option<String>,
    /// Root of the move's destination (only with [`TreeReason::MovedAway`]).
    pub move_dst_op_root: Option<String>,
}

/// One conflict of a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    /// Text conflict.
    Text(TextConflict),
    /// Property conflict.
    Prop(PropConflict),
    /// Tree conflict.
    Tree(TreeConflict),
    /// A kind this version does not know (`reject`, `obstructed`), kept verbatim.
    Other(Skel),
}

/// A node's complete conflict description.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConflictSkel {
    /// Operation and origins; `None` while the skel is still being built.
    pub operation: Option<OperationInfo>,
    /// Conflicts, newest first (svn prepends).
    pub conflicts: Vec<Conflict>,
}

fn marker(s: &Skel) -> Result<Option<String>> {
    match s {
        Skel::List(_) => Ok(None),
        a => atom_string(a, "marker").map(Some),
    }
}

fn opt_marker(m: &Option<String>) -> Skel {
    m.as_deref().map_or_else(|| Skel::list(vec![]), atom)
}

fn props(s: &Skel) -> Result<Option<Vec<PropPair>>> {
    match s {
        Skel::Atom(_) => Ok(None),
        list => parse_proplist(list).map(Some),
    }
}

fn opt_props(p: &Option<Vec<PropPair>>) -> Skel {
    p.as_ref().map_or_else(|| atom(""), |p| unparse_proplist(p))
}

impl Conflict {
    fn parse(s: &Skel) -> Result<Self> {
        let items = list(s, "conflict")?;
        let Some(kind) = items.first().and_then(Skel::as_atom) else {
            return Err(bad("conflict has no kind"));
        };
        match kind {
            b"text" => {
                let [_, markers] = items else {
                    return Err(bad("text conflict needs 2 elements"));
                };
                let [original, mine, their] = list(markers, "text markers")? else {
                    return Err(bad("text conflict needs 3 markers"));
                };
                Ok(Self::Text(TextConflict {
                    original: marker(original)?,
                    mine: marker(mine)?,
                    their: marker(their)?,
                }))
            }
            b"prop" => {
                let [_, markers, names, old, mine, their] = items else {
                    return Err(bad("prop conflict needs 6 elements"));
                };
                Ok(Self::Prop(PropConflict {
                    markers: list(markers, "prop markers")?
                        .iter()
                        .map(|m| atom_string(m, "prop marker"))
                        .collect::<Result<_>>()?,
                    names: list(names, "prop names")?
                        .iter()
                        .map(|n| atom_string(n, "prop name"))
                        .collect::<Result<_>>()?,
                    old_props: props(old)?,
                    mine_props: props(mine)?,
                    their_props: props(their)?,
                }))
            }
            b"tree" => {
                if items.len() < 4 || items.len() > 6 {
                    return Err(bad("tree conflict needs 4..=6 elements"));
                }
                let reason = TreeReason::from_token(atom_string(&items[2], "tree reason")?);
                let action = TreeAction::from_token(atom_string(&items[3], "tree action")?);
                let optional = |i: usize| {
                    items
                        .get(i)
                        .map(|s| atom_string(s, "move root"))
                        .transpose()
                };
                Ok(Self::Tree(TreeConflict {
                    reason,
                    action,
                    move_src_op_root: optional(4)?,
                    move_dst_op_root: optional(5)?,
                }))
            }
            _ => Ok(Self::Other(s.clone())),
        }
    }

    fn to_skel(&self) -> Skel {
        match self {
            Self::Text(t) => Skel::list(vec![
                atom(KIND_TEXT),
                Skel::list(vec![
                    opt_marker(&t.original),
                    opt_marker(&t.mine),
                    opt_marker(&t.their),
                ]),
            ]),
            Self::Prop(p) => Skel::list(vec![
                atom(KIND_PROP),
                Skel::list(p.markers.iter().map(|m| atom(m)).collect::<Vec<_>>()),
                Skel::list(p.names.iter().map(|n| atom(n)).collect::<Vec<_>>()),
                opt_props(&p.old_props),
                opt_props(&p.mine_props),
                opt_props(&p.their_props),
            ]),
            Self::Tree(t) => {
                let mut v = vec![
                    atom(KIND_TREE),
                    Skel::list(vec![]),
                    atom(t.reason.token()),
                    atom(t.action.token()),
                ];
                v.extend(t.move_src_op_root.iter().map(|p| atom(p)));
                v.extend(t.move_dst_op_root.iter().map(|p| atom(p)));
                Skel::list(v)
            }
            Self::Other(s) => s.clone(),
        }
    }
}

impl ConflictSkel {
    /// Interprets a parsed skel.
    pub fn from_skel(skel: &Skel) -> Result<Self> {
        let [why, conflicts] = list(skel, "conflict skel")? else {
            return Err(bad("expected (WHY CONFLICTS)"));
        };
        let why = list(why, "operation")?;
        let operation = match why {
            [] => None,
            [op, origins] => {
                let [left, right] = list(origins, "origins")? else {
                    return Err(bad("operation needs 2 origins"));
                };
                Some(OperationInfo {
                    operation: Operation::from_token(atom_string(op, "operation")?),
                    left: Origin::parse(left)?,
                    right: Origin::parse(right)?,
                })
            }
            _ => return Err(bad("operation needs a name and origins")),
        };
        Ok(Self {
            operation,
            conflicts: list(conflicts, "conflict list")?
                .iter()
                .map(Conflict::parse)
                .collect::<Result<_>>()?,
        })
    }

    /// Parses the stored `conflict_data` bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        Self::from_skel(&Skel::parse(bytes)?)
    }

    /// Builds the skel exactly as svn lays it out.
    pub fn to_skel(&self) -> Skel {
        let why = match &self.operation {
            None => Skel::list(vec![]),
            Some(op) => Skel::list(vec![
                atom(op.operation.token()),
                Skel::list(vec![op.left.to_skel(), op.right.to_skel()]),
            ]),
        };
        Skel::list(vec![
            why,
            Skel::list(
                self.conflicts
                    .iter()
                    .map(Conflict::to_skel)
                    .collect::<Vec<_>>(),
            ),
        ])
    }

    /// Serialized form for `ACTUAL_NODE.conflict_data`.
    pub fn encode(&self) -> Vec<u8> {
        self.to_skel().unparse()
    }

    /// `svn_wc__conflict_skel_is_complete`: operation set and at least one conflict.
    pub fn is_complete(&self) -> bool {
        self.operation.is_some() && !self.conflicts.is_empty()
    }

    /// The text conflict, if any.
    pub fn text(&self) -> Option<&TextConflict> {
        self.conflicts.iter().find_map(|c| {
            if let Conflict::Text(t) = c {
                Some(t)
            } else {
                None
            }
        })
    }

    /// The property conflict, if any.
    pub fn prop(&self) -> Option<&PropConflict> {
        self.conflicts.iter().find_map(|c| {
            if let Conflict::Prop(p) = c {
                Some(p)
            } else {
                None
            }
        })
    }

    /// The tree conflict, if any.
    pub fn tree(&self) -> Option<&TreeConflict> {
        self.conflicts.iter().find_map(|c| {
            if let Conflict::Tree(t) = c {
                Some(t)
            } else {
                None
            }
        })
    }

    /// Adds a conflict in front of the others, like svn. Each kind may appear once.
    pub fn add(&mut self, conflict: Conflict) -> Result<()> {
        let kind = |c: &Conflict| std::mem::discriminant(c);
        if !matches!(conflict, Conflict::Other(_))
            && self.conflicts.iter().any(|c| kind(c) == kind(&conflict))
        {
            return Err(SvnError::new(
                ErrorCode::Internal,
                "a conflict of this kind is already recorded",
            ));
        }
        self.conflicts.insert(0, conflict);
        Ok(())
    }

    /// `svn_wc__conflict_skel_resolve`: drops the resolved conflicts and returns `true` when
    /// nothing is left. `resolve_prop` is a property name, or `""` for all of them.
    pub fn resolve(
        &mut self,
        resolve_text: bool,
        resolve_prop: Option<&str>,
        resolve_tree: bool,
    ) -> Result<bool> {
        if self.operation.is_none() {
            return Err(SvnError::new(
                ErrorCode::Internal,
                "not a completed conflict skel",
            ));
        }
        self.conflicts.retain_mut(|c| match c {
            Conflict::Text(_) => !resolve_text,
            Conflict::Tree(_) => !resolve_tree,
            Conflict::Prop(p) => {
                match resolve_prop {
                    Some("") => p.names.clear(),
                    Some(name) => {
                        if let Some(i) = p.names.iter().position(|n| n == name) {
                            p.names.remove(i);
                        }
                    }
                    None => return true,
                }
                !p.names.is_empty()
            }
            Conflict::Other(_) => true,
        });
        Ok(!self.is_complete())
    }
}

#[cfg(test)]
mod tests;

//! Support for a [`FakeRaSession`](super::FakeRaSession) rooted below the repository root
//! (`FakeRaSession::at`): path translation and a commit editor that opens the session
//! directory first, like a commit through a session at that URL.

use async_trait::async_trait;
use svn_types::{
    Checksum, DirId, ErrorCode, FileId, PropChange, RelPath, RepoUrl, Result, Revnum, SvnError,
};

use super::repo::Tree;
use crate::commit::{CommitEditor, CommitInfo, ReplayableSource};

/// `base/rel`.
pub(super) fn join(base: &RelPath, rel: &RelPath) -> Result<RelPath> {
    if base.is_root() {
        Ok(rel.clone())
    } else if rel.is_root() {
        Ok(base.clone())
    } else {
        RelPath::new(&format!("{base}/{rel}"))
    }
}

/// `path` relative to `base`; `path` must be at or below `base`.
fn strip(path: &RelPath, base: &RelPath) -> Result<RelPath> {
    if !path.starts_with(base) {
        return Err(SvnError::new(
            ErrorCode::Internal,
            format!("{path} is not below {base}"),
        ));
    }
    let rest = path.as_str().get(base.as_str().len()..).unwrap_or("");
    RelPath::new(rest.trim_start_matches('/'))
}

/// The part of `tree` at or below `base`, with paths relative to `base`.
pub(super) fn subtree(tree: &Tree, base: &RelPath) -> Result<Tree> {
    if base.is_root() {
        return Ok(tree.clone());
    }
    tree.iter()
        .filter(|(k, _)| k.starts_with(base))
        .map(|(k, n)| Ok((strip(k, base)?, n.clone())))
        .collect()
}

fn decode(seg: &str) -> Result<String> {
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = seg
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| SvnError::new(ErrorCode::UnsafePath, "bad escape in URL"))?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| SvnError::new(ErrorCode::UnsafePath, "URL is not UTF-8"))
}

/// Repository-relative path of `url`, which must be on the origin of `root` and at or below it.
pub(super) fn rel_of(root: &RepoUrl, url: &RepoUrl) -> Result<RelPath> {
    let (r, u) = (root.as_url(), url.as_url());
    let outside = || {
        SvnError::new(
            ErrorCode::UnsafePath,
            format!("{url} is outside the repository {root}"),
        )
    };
    if r.origin() != u.origin() {
        return Err(outside());
    }
    let rs: Vec<&str> = r.path().split('/').filter(|s| !s.is_empty()).collect();
    let us: Vec<&str> = u.path().split('/').filter(|s| !s.is_empty()).collect();
    if us.len() < rs.len() || us[..rs.len()] != rs[..] {
        return Err(outside());
    }
    let rest = us[rs.len()..]
        .iter()
        .map(|s| decode(s))
        .collect::<Result<Vec<_>>>()?;
    RelPath::new(&rest.join("/"))
}

/// Commit editor of a session below the repository root: `open_root` opens the session
/// directory (and its ancestors) in the repository-rooted editor, and every path is
/// translated from session-relative to repository-relative.
pub(super) struct PrefixedCommitEditor {
    pub inner: Box<dyn CommitEditor>,
    pub prefix: RelPath,
    /// Directories opened by `open_root`, root first; the last one is the session directory.
    pub chain: Vec<DirId>,
}

impl PrefixedCommitEditor {
    fn abs(&self, path: &RelPath) -> Result<RelPath> {
        join(&self.prefix, path)
    }
}

/// `(path, token)` lock pairs translated from session-relative to repository-relative.
pub(super) fn lock_tokens(
    prefix: &RelPath,
    tokens: &[(String, String)],
) -> Result<Vec<(String, String)>> {
    tokens
        .iter()
        .map(|(p, t)| {
            let rel = RelPath::new(p.trim_start_matches('/'))?;
            Ok((join(prefix, &rel)?.as_str().to_owned(), t.clone()))
        })
        .collect()
}

#[async_trait]
impl CommitEditor for PrefixedCommitEditor {
    async fn open_root(&mut self, base_rev: Option<Revnum>) -> Result<DirId> {
        let mut dir = self.inner.open_root(None).await?;
        self.chain.push(dir);
        let components: Vec<String> = self.prefix.components().map(str::to_owned).collect();
        let mut path = RelPath::root();
        for (i, c) in components.iter().enumerate() {
            path = path.child(c)?;
            let base = if i + 1 == components.len() {
                base_rev
            } else {
                None
            };
            dir = self.inner.open_directory(&path, dir, base).await?;
            self.chain.push(dir);
        }
        Ok(dir)
    }

    async fn add_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> Result<DirId> {
        let path = self.abs(path)?;
        self.inner.add_directory(&path, parent, copy_from).await
    }

    async fn open_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<DirId> {
        let path = self.abs(path)?;
        self.inner.open_directory(&path, parent, base_rev).await
    }

    async fn add_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> Result<FileId> {
        let path = self.abs(path)?;
        self.inner.add_file(&path, parent, copy_from).await
    }

    async fn open_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<FileId> {
        let path = self.abs(path)?;
        self.inner.open_file(&path, parent, base_rev).await
    }

    async fn apply_text(
        &mut self,
        file: FileId,
        base: Option<ReplayableSource>,
        target: ReplayableSource,
        base_md5: Option<Checksum>,
        result_md5: Checksum,
    ) -> Result<()> {
        self.inner
            .apply_text(file, base, target, base_md5, result_md5)
            .await
    }

    async fn change_dir_prop(&mut self, dir: DirId, change: PropChange) -> Result<()> {
        self.inner.change_dir_prop(dir, change).await
    }

    async fn change_file_prop(&mut self, file: FileId, change: PropChange) -> Result<()> {
        self.inner.change_file_prop(file, change).await
    }

    async fn delete_entry(
        &mut self,
        path: &RelPath,
        rev: Option<Revnum>,
        parent: DirId,
    ) -> Result<()> {
        let path = self.abs(path)?;
        self.inner.delete_entry(&path, rev, parent).await
    }

    async fn close_file(&mut self, file: FileId) -> Result<()> {
        self.inner.close_file(file).await
    }

    async fn close_directory(&mut self, dir: DirId) -> Result<()> {
        if self.chain.last() == Some(&dir) {
            // Closing the session directory closes the directories `open_root` opened above it.
            while let Some(d) = self.chain.pop() {
                self.inner.close_directory(d).await?;
            }
            return Ok(());
        }
        self.inner.close_directory(dir).await
    }

    async fn close_edit(self: Box<Self>) -> Result<CommitInfo> {
        self.inner.close_edit().await
    }

    async fn abort_edit(self: Box<Self>) -> Result<()> {
        self.inner.abort_edit().await
    }
}

/// Boxes `inner` so it works below `prefix` (returned unchanged for the root).
pub(super) fn wrap(inner: Box<dyn CommitEditor>, prefix: &RelPath) -> Box<dyn CommitEditor> {
    if prefix.is_root() {
        return inner;
    }
    Box::new(PrefixedCommitEditor {
        inner,
        prefix: prefix.clone(),
        chain: Vec::new(),
    })
}

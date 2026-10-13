//! Driving a [`CommitEditor`] with the harvested committables.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use svn_ra_api::{CommitEditor, ReplayableSource};
use svn_types::{
    CancelToken, Checksum, DirId, ErrorCode, RelPath, RepoUrl, Result, Revnum, SvnError,
};

use super::collect::{Committable, ItemAction, ItemKind};
use super::journal::JournalText;

/// Upload data of a file.
#[derive(Debug, Clone)]
pub struct PreparedText {
    /// Repository-normal text to upload (re-readable).
    pub target: ReplayableSource,
    /// Pristine to compute the delta against (`None` = send the full text).
    pub base: Option<ReplayableSource>,
    /// MD5 of the pristine, for the server's base check.
    pub base_md5: Option<Checksum>,
    /// MD5 of the new text.
    pub result_md5: Checksum,
    /// What the journal remembers about the text.
    pub record: JournalText,
}

/// A committable plus what is needed to send it.
#[derive(Debug, Clone)]
pub struct DriveItem {
    /// The change.
    pub item: Committable,
    /// File upload data when `item.needs_text`.
    pub text: Option<PreparedText>,
}

/// Static parameters of a drive.
pub struct DriveContext<'a> {
    /// Repository root, to build copy-from URLs.
    pub repos_root: &'a RepoUrl,
    /// Repository path of the session URL; editor paths are relative to it.
    pub session_prefix: &'a str,
    /// BASE revisions of the intermediate directories, by repository path.
    pub dir_revisions: &'a BTreeMap<String, u64>,
    /// Cancellation, checked between paths.
    pub cancel: &'a CancelToken,
}

#[derive(Default)]
struct Trie {
    item: Option<usize>,
    repos_path: String,
    children: BTreeMap<String, Trie>,
}

fn internal(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::Internal, msg)
}

/// Path of `repos` relative to the session prefix.
pub fn editor_path(prefix: &str, repos: &str) -> Result<RelPath> {
    if prefix.is_empty() {
        return RelPath::new(repos);
    }
    if repos == prefix {
        return Ok(RelPath::root());
    }
    match repos.strip_prefix(prefix).and_then(|r| r.strip_prefix('/')) {
        Some(r) => RelPath::new(r),
        None => Err(SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!("'{repos}' is outside the session URL path '{prefix}'"),
        )),
    }
}

fn build_trie(items: &[DriveItem], prefix: &str) -> Result<Trie> {
    let mut root = Trie::default();
    for (i, di) in items.iter().enumerate() {
        let rel = editor_path(prefix, &di.item.repos_relpath)?;
        let mut node = &mut root;
        let mut path = String::new();
        for comp in rel.components() {
            path = if path.is_empty() {
                comp.to_owned()
            } else {
                format!("{path}/{comp}")
            };
            let full = if prefix.is_empty() {
                path.clone()
            } else {
                format!("{prefix}/{path}")
            };
            node = node
                .children
                .entry(comp.to_owned())
                .or_insert_with(|| Trie {
                    repos_path: full,
                    ..Trie::default()
                });
        }
        if node.item.is_some() {
            return Err(internal(format!("duplicate committable for '{rel}'")));
        }
        node.item = Some(i);
    }
    Ok(root)
}

fn copy_arg(ctx: &DriveContext<'_>, di: &DriveItem) -> Result<Option<(String, Revnum)>> {
    let Some(c) = &di.item.copy_from else {
        return Ok(None);
    };
    let url = ctx.repos_root.join(&RelPath::new(&c.repos_relpath)?)?;
    Ok(Some((url.as_url().as_str().to_owned(), Revnum(c.revision))))
}

/// Sends every committable through `editor`. The caller closes or aborts the editor.
pub async fn drive(
    editor: &mut dyn CommitEditor,
    items: &[DriveItem],
    ctx: &DriveContext<'_>,
) -> Result<()> {
    let trie = build_trie(items, ctx.session_prefix)?;
    // A changed session root is opened at its BASE revision, so the server's out-of-date check
    // covers its property changes (svn's `do_item_commit` opens it with `item->revision`).
    let root_rev = trie.item.and_then(|i| items[i].item.base_rev).map(Revnum);
    let root = editor.open_root(root_rev).await?;
    if let Some(i) = trie.item {
        for change in &items[i].item.prop_changes {
            editor.change_dir_prop(root, change.clone()).await?;
        }
    }
    drive_children(editor, items, ctx, &trie, root).await?;
    editor.close_directory(root).await
}

type BoxFut<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

fn drive_children<'a>(
    editor: &'a mut dyn CommitEditor,
    items: &'a [DriveItem],
    ctx: &'a DriveContext<'a>,
    node: &'a Trie,
    dir: DirId,
) -> BoxFut<'a> {
    Box::pin(async move {
        for child in node.children.values() {
            ctx.cancel.check()?;
            let path = editor_path(ctx.session_prefix, &child.repos_path)?;
            let Some(idx) = child.item else {
                // Untouched directory on the way to a change.
                let rev = ctx.dir_revisions.get(&child.repos_path).map(|r| Revnum(*r));
                let d = editor.open_directory(&path, dir, rev).await?;
                drive_children(editor, items, ctx, child, d).await?;
                editor.close_directory(d).await?;
                continue;
            };
            let di = &items[idx];
            let it = &di.item;
            let base_rev = it.base_rev.map(Revnum);
            if matches!(it.action, ItemAction::Delete | ItemAction::Replace) {
                editor.delete_entry(&path, base_rev, dir).await?;
            }
            if it.action == ItemAction::Delete {
                continue;
            }
            let adding = matches!(it.action, ItemAction::Add | ItemAction::Replace);
            match it.kind {
                ItemKind::Dir => {
                    let d = if adding {
                        editor
                            .add_directory(&path, dir, copy_arg(ctx, di)?.as_ref().map(as_ref))
                            .await?
                    } else {
                        editor.open_directory(&path, dir, base_rev).await?
                    };
                    for change in &it.prop_changes {
                        editor.change_dir_prop(d, change.clone()).await?;
                    }
                    drive_children(editor, items, ctx, child, d).await?;
                    editor.close_directory(d).await?;
                }
                ItemKind::File => {
                    let f = if adding {
                        editor
                            .add_file(&path, dir, copy_arg(ctx, di)?.as_ref().map(as_ref))
                            .await?
                    } else {
                        editor.open_file(&path, dir, base_rev).await?
                    };
                    if let Some(t) = &di.text {
                        editor
                            .apply_text(
                                f,
                                t.base.clone(),
                                t.target.clone(),
                                t.base_md5,
                                t.result_md5,
                            )
                            .await?;
                    }
                    for change in &it.prop_changes {
                        editor.change_file_prop(f, change.clone()).await?;
                    }
                    editor.close_file(f).await?;
                }
            }
        }
        Ok(())
    })
}

fn as_ref(c: &(String, Revnum)) -> (&str, Revnum) {
    (c.0.as_str(), c.1)
}

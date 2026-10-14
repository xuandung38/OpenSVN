//! Detection of the trunk/branches/tags convention. Subversion has no real branches; this only
//! looks for the common folder names, at the repository root or one project level below it.

use async_trait::async_trait;
use opensvn_ipc::{AppError, BranchKind, BranchRef, EntryKind, Layout, LayoutDetection};

use crate::engine::{EngineFacade, OpContext};

/// Lists the sub-directories of a repository URL.
#[async_trait]
pub trait DirLister: Send + Sync {
    /// Names of the directories directly below `url` (empty when `url` does not exist).
    async fn dirs(&self, url: &str) -> Result<Vec<String>, AppError>;
}

/// Lists directories through an engine. Uses an unattended context: detection never prompts.
pub struct EngineDirs<'a>(pub &'a dyn EngineFacade);

#[async_trait]
impl DirLister for EngineDirs<'_> {
    async fn dirs(&self, url: &str) -> Result<Vec<String>, AppError> {
        match self.0.repo_list(url, None, &OpContext::background()).await {
            Ok(entries) => Ok(entries
                .into_iter()
                .filter(|e| e.kind == EntryKind::Dir)
                .map(|e| e.name)
                .collect()),
            // A folder that does not exist simply has no layout below it.
            Err(e) if e.code == svn_types::ErrorCode::PathNotFound => Ok(Vec::new()),
            Err(e) => Err(AppError::from(&e)),
        }
    }
}

/// Patterns of a layout one project level below the repository root.
pub fn project_layout() -> Layout {
    Layout::Custom {
        trunk: "/{project}/trunk".to_owned(),
        branches: vec!["/{project}/branches".to_owned()],
        tags: vec!["/{project}/tags".to_owned()],
    }
}

/// True when `url` could sit in a trunk/branches/tags layout at the repository root or one project
/// level below it. Lets callers skip the server round trips for folder-style working copies.
pub fn may_be_in_layout(repo_root_url: &str, url: &str) -> bool {
    let rel = url
        .trim_end_matches('/')
        .strip_prefix(repo_root_url.trim_end_matches('/'))
        .unwrap_or("");
    let segments: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    let at = |i: usize| match segments.get(i) {
        Some(&"trunk") => true,
        Some(&("branches" | "tags")) => segments.len() > i + 1,
        _ => false,
    };
    at(0) || at(1)
}

fn join(base: &str, name: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), name)
}

/// True when `dirs` has `trunk` and at least one of `branches` / `tags`.
fn looks_like_layout(dirs: &[String]) -> bool {
    let has = |n: &str| dirs.iter().any(|d| d == n);
    has("trunk") && (has("branches") || has("tags"))
}

/// Which branch `segments` (path below the layout base) point into.
fn current_branch(base_url: &str, segments: &[&str]) -> Option<BranchRef> {
    match segments {
        ["trunk", ..] => Some(BranchRef {
            kind: BranchKind::Trunk,
            name: "trunk".to_owned(),
            url: join(base_url, "trunk"),
        }),
        [kind @ ("branches" | "tags"), name, ..] => Some(BranchRef {
            kind: if *kind == "branches" {
                BranchKind::Branch
            } else {
                BranchKind::Tag
            },
            name: (*name).to_owned(),
            url: join(&join(base_url, kind), name),
        }),
        _ => None,
    }
}

/// Detects the layout that contains `url` inside the repository at `repo_root_url`.
pub async fn detect(
    lister: &dyn DirLister,
    repo_root_url: &str,
    url: &str,
) -> Result<LayoutDetection, AppError> {
    let root = repo_root_url.trim_end_matches('/');
    let rel = url
        .trim_end_matches('/')
        .strip_prefix(root)
        .unwrap_or("")
        .trim_start_matches('/');
    let segments: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();

    // Layout at the repository root: /trunk, /branches, /tags.
    if looks_like_layout(&lister.dirs(root).await?) {
        return Ok(LayoutDetection {
            found: true,
            layout: Some(Layout::Auto),
            current: current_branch(root, &segments),
        });
    }
    // One project level below the root: /{project}/trunk, ...
    if let Some((project, below)) = segments.split_first() {
        let project_url = join(root, project);
        if looks_like_layout(&lister.dirs(&project_url).await?) {
            return Ok(LayoutDetection {
                found: true,
                layout: Some(project_layout()),
                current: current_branch(&project_url, below),
            });
        }
    }
    Ok(LayoutDetection {
        found: false,
        layout: None,
        current: None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Tree(HashMap<&'static str, Vec<&'static str>>);

    #[async_trait]
    impl DirLister for Tree {
        async fn dirs(&self, url: &str) -> Result<Vec<String>, AppError> {
            Ok(self
                .0
                .get(url)
                .map(|v| v.iter().map(|s| (*s).to_owned()).collect())
                .unwrap_or_default())
        }
    }

    const ROOT: &str = "https://svn.test.local/repos/r";

    #[tokio::test]
    async fn root_level_layout_is_auto_and_locates_the_current_branch() {
        let t = Tree(HashMap::from([(
            ROOT,
            vec!["trunk", "branches", "tags", "docs"],
        )]));
        let d = detect(&t, ROOT, &format!("{ROOT}/branches/feature-x/src"))
            .await
            .unwrap();
        assert!(d.found);
        assert_eq!(d.layout, Some(Layout::Auto));
        let cur = d.current.unwrap();
        assert_eq!(
            (cur.kind, cur.name.as_str()),
            (BranchKind::Branch, "feature-x")
        );
        assert_eq!(cur.url, format!("{ROOT}/branches/feature-x"));
        let d = detect(&t, ROOT, &format!("{ROOT}/trunk")).await.unwrap();
        assert_eq!(d.current.unwrap().kind, BranchKind::Trunk);
        let d = detect(&t, ROOT, ROOT).await.unwrap();
        assert!(d.found && d.current.is_none());
    }

    #[test]
    fn only_urls_shaped_like_a_layout_are_worth_detecting() {
        for (url, expected) in [
            (format!("{ROOT}/trunk"), true),
            (format!("{ROOT}/trunk/src"), true),
            (format!("{ROOT}/branches/x"), true),
            (format!("{ROOT}/app/tags/v1/"), true),
            (format!("{ROOT}/branches"), false),
            (format!("{ROOT}/app/lib/trunk"), false),
            (format!("{ROOT}/docs"), false),
            (ROOT.to_owned(), false),
        ] {
            assert_eq!(may_be_in_layout(ROOT, &url), expected, "{url}");
        }
    }

    #[tokio::test]
    async fn project_level_layout_uses_the_project_placeholder() {
        let t = Tree(HashMap::from([
            (ROOT, vec!["app", "lib"]),
            ("https://svn.test.local/repos/r/app", vec!["trunk", "tags"]),
        ]));
        let d = detect(&t, ROOT, &format!("{ROOT}/app/tags/v1/x"))
            .await
            .unwrap();
        assert!(d.found);
        assert!(
            matches!(d.layout, Some(Layout::Custom { ref trunk, .. }) if trunk == "/{project}/trunk")
        );
        let cur = d.current.unwrap();
        assert_eq!((cur.kind, cur.name.as_str()), (BranchKind::Tag, "v1"));
        assert_eq!(cur.url, format!("{ROOT}/app/tags/v1"));
    }

    #[tokio::test]
    async fn plain_folder_repository_has_no_layout() {
        let t = Tree(HashMap::from([
            (ROOT, vec!["a", "b"]),
            ("https://svn.test.local/repos/r/a", vec!["x"]),
        ]));
        let d = detect(&t, ROOT, &format!("{ROOT}/a/x")).await.unwrap();
        assert!(!d.found && d.layout.is_none() && d.current.is_none());
        // a trunk without branches or tags is not enough
        let t = Tree(HashMap::from([(ROOT, vec!["trunk"])]));
        assert!(!detect(&t, ROOT, ROOT).await.unwrap().found);
    }
}

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
            Ok(listing) => Ok(listing
                .entries
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

/// Decodes `%XX` escapes of a URL path segment for display. Invalid escapes or bytes that are not
/// UTF-8 leave the segment as it is.
pub fn decode_segment(seg: &str) -> String {
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).and_then(|b| hex(*b)),
                bytes.get(i + 2).and_then(|b| hex(*b)),
            )
        {
            #[allow(clippy::cast_possible_truncation)]
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| seg.to_owned())
}

/// Raw (still encoded) path segments of `url` below `repo_root_url`; `None` when `url` is not inside
/// the repository.
fn segments_below<'a>(repo_root_url: &str, url: &'a str) -> Option<Vec<&'a str>> {
    let root = repo_root_url.trim_end_matches('/');
    let rest = url.trim_end_matches('/').strip_prefix(root)?;
    if !(rest.is_empty() || rest.starts_with('/')) {
        return None;
    }
    Some(rest.split('/').filter(|s| !s.is_empty()).collect())
}

/// Number of leading `segs` matched by `pattern` (`/a/{project}/b`; `{project}` is any one
/// segment), compared on the decoded segment.
fn match_pattern(pattern: &str, segs: &[&str]) -> Option<usize> {
    let parts: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() || parts.len() > segs.len() {
        return None;
    }
    parts
        .iter()
        .zip(segs)
        .all(|(p, s)| *p == PROJECT || *p == decode_segment(s))
        .then_some(parts.len())
}

const PROJECT: &str = "{project}";

/// Branch that `segs` point into under one layout, or `None`.
fn branch_in(
    root: &str,
    segs: &[&str],
    trunk: &str,
    branches: &[String],
    tags: &[String],
) -> Option<BranchRef> {
    let url_of = |n: usize| join(root, &segs[..n].join("/"));
    for (patterns, kind) in [(branches, BranchKind::Branch), (tags, BranchKind::Tag)] {
        for p in patterns {
            if let Some(n) = match_pattern(p, segs)
                && let Some(name) = segs.get(n)
            {
                return Some(BranchRef {
                    kind,
                    name: decode_segment(name),
                    url: url_of(n + 1),
                });
            }
        }
    }
    let n = match_pattern(trunk, segs)?;
    Some(BranchRef {
        kind: BranchKind::Trunk,
        name: decode_segment(segs[n - 1]),
        url: url_of(n),
    })
}

/// Branch of a working copy at `url`, from the URL and the stored layout only (no server round
/// trip). With no stored layout (or `Auto`), the trunk/branches/tags convention at the repository
/// root, then one project level below it. `Layout::None` never has a branch.
pub fn branch_from_url(
    repo_root_url: &str,
    url: &str,
    layout: Option<&Layout>,
) -> Option<BranchRef> {
    let segs = segments_below(repo_root_url, url)?;
    let root = repo_root_url.trim_end_matches('/');
    let std = |b: &str, t: &str| (vec![b.to_owned()], vec![t.to_owned()]);
    match layout {
        Some(Layout::None) => None,
        Some(Layout::Custom {
            trunk,
            branches,
            tags,
        }) => branch_in(root, &segs, trunk, branches, tags),
        Some(Layout::Auto) | None => {
            let (b, t) = std("/branches", "/tags");
            branch_in(root, &segs, "/trunk", &b, &t).or_else(|| {
                let (b, t) = std("/{project}/branches", "/{project}/tags");
                branch_in(root, &segs, "/{project}/trunk", &b, &t)
            })
        }
    }
}

/// Layout to list branches with when none is stored, from a URL inside it: the root-level
/// convention, or the one project folder the URL is in (so only that project is listed).
pub fn layout_near(repo_root_url: &str, url: &str) -> Layout {
    let segs = segments_below(repo_root_url, url).unwrap_or_default();
    let is_std = |i: usize| {
        segs.get(i)
            .is_some_and(|s| matches!(*s, "trunk" | "branches" | "tags"))
    };
    match segs.first() {
        Some(project) if !is_std(0) && is_std(1) => {
            let project = decode_segment(project);
            Layout::Custom {
                trunk: format!("/{project}/trunk"),
                branches: vec![format!("/{project}/branches")],
                tags: vec![format!("/{project}/tags")],
            }
        }
        _ => Layout::Auto,
    }
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
            name: decode_segment(name),
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
    fn branch_from_url_follows_the_convention_without_the_server() {
        let b = |url: &str, l: Option<&Layout>| {
            branch_from_url(ROOT, &format!("{ROOT}{url}"), l)
                .map(|b| (b.kind, b.name, b.url.trim_start_matches(ROOT).to_owned()))
        };
        let r = |k, n: &str, u: &str| Some((k, n.to_owned(), u.to_owned()));
        assert_eq!(
            b("/trunk/src", None),
            r(BranchKind::Trunk, "trunk", "/trunk")
        );
        assert_eq!(
            b("/branches/feature-x/a", Some(&Layout::Auto)),
            r(BranchKind::Branch, "feature-x", "/branches/feature-x")
        );
        assert_eq!(
            b("/app/tags/v1/", None),
            r(BranchKind::Tag, "v1", "/app/tags/v1")
        );
        // Display names are decoded; the URL stays as it is.
        assert_eq!(
            b("/branches/f%C3%A9ature%20x", None),
            r(
                BranchKind::Branch,
                "féature x",
                "/branches/f%C3%A9ature%20x"
            )
        );
        for folder in ["/branches", "/docs", "/app/lib/trunk", "", "/Trunk"] {
            assert_eq!(b(folder, None), None, "{folder}");
        }
        assert_eq!(b("/trunk", Some(&Layout::None)), None);
        assert_eq!(
            branch_from_url(ROOT, "https://other.test/r/trunk", None),
            None
        );
        assert_eq!(branch_from_url(ROOT, &format!("{ROOT}x/trunk"), None), None);

        let custom = Layout::Custom {
            trunk: "/main".into(),
            branches: vec!["/dev".into(), "/{project}/feat".into()],
            tags: vec!["/rel".into()],
        };
        let c = Some(&custom);
        assert_eq!(b("/main/x", c), r(BranchKind::Trunk, "main", "/main"));
        assert_eq!(b("/dev/a/b", c), r(BranchKind::Branch, "a", "/dev/a"));
        assert_eq!(b("/p/feat/b", c), r(BranchKind::Branch, "b", "/p/feat/b"));
        assert_eq!(b("/rel/1.0", c), r(BranchKind::Tag, "1.0", "/rel/1.0"));
        assert_eq!(
            b("/trunk", c),
            None,
            "a custom layout replaces the convention"
        );
    }

    #[test]
    fn layout_near_limits_listing_to_the_project_of_the_url() {
        assert_eq!(layout_near(ROOT, &format!("{ROOT}/trunk")), Layout::Auto);
        assert_eq!(layout_near(ROOT, &format!("{ROOT}/docs")), Layout::Auto);
        assert_eq!(
            layout_near(ROOT, &format!("{ROOT}/app/branches/x")),
            Layout::Custom {
                trunk: "/app/trunk".into(),
                branches: vec!["/app/branches".into()],
                tags: vec!["/app/tags".into()],
            }
        );
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

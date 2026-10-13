//! Working copy reads through `svn-client`: status, working copy diff and remote status.
//!
//! * `status` is `svn_client::status` without unmodified nodes. `svn-client` already lists the
//!   unmodified nodes the facade asks for (locked, changelisted, switched), so no second pass is
//!   needed; statuses are sorted by path like the fake engine's.
//! * `diff_wc` compares two sides in repository-normal form: the pristine text (or the copy
//!   source of a copied node) or the file at a revision read from the repository, against the
//!   working file with keywords and line endings normalized as a commit would send it. Text is
//!   shown through the shared `engine::text` rules (binary, display budget), sides above
//!   [`CAT_LIMIT`] are flagged `too_large` unless the caller forces them, and a binary
//!   `svn:mime-type` wins over content sniffing. Property changes are listed for files and
//!   directories.
//! * `status_remote` runs `svn_client::status_remote` on a session anchored at the working copy
//!   URL (with the interactivity of the call) and counts the revisions the working copy lacks,
//!   capped at [`INCOMING_CAP`].

use std::fs;
use std::io::Read;
use std::path::Path;

use opensvn_ipc::{FileDiff, FileStatus, Rev};
use svn_client::StatusOptions;
use svn_client::wc::inspect::{prop_value, working_repo_normal};
use svn_ra_api::{LogRequest, RaSession};
use svn_types::{CancelToken, Depth, ErrorCode, NodeKind, RelPath, RepoUrl, Revnum, SvnError};
use svn_wc::db::{Node, Presence, Props, WcDb};
use svn_wc::pristine::Sha1Digest;

use super::remote::CAT_LIMIT;
use super::text::CappedBuf;
use super::{ConnectOptions, RealEngine, wc_map, wc_read};
use crate::engine::context::OpContext;
use crate::engine::facade::{DiffOptions, EngineResult, RemoteScan, StatusQuery, WcRef};
use crate::engine::text::{DiffSide, file_diff, percent_decode};

/// Largest number of incoming revisions `status_remote` counts.
pub(super) const INCOMING_CAP: u32 = 1000;

/// Content of one side of a working copy diff.
#[derive(Debug)]
enum Content {
    /// The node does not exist on that side.
    Absent,
    /// The bytes in repository-normal form.
    Bytes(Vec<u8>),
    /// Larger than the inline limit; not read.
    TooLarge,
}

impl Content {
    fn side(self, label: String) -> DiffSide {
        match self {
            Content::Bytes(b) => Some((b, label)),
            Content::Absent | Content::TooLarge => None,
        }
    }
}

/// What the working copy knows about the diffed node.
struct LocalSides {
    kind: NodeKind,
    /// Pristine side (BASE or the copy source).
    base: Content,
    /// Properties of the pristine side.
    base_props: Props,
    /// Working side.
    working: Content,
    /// Working properties (empty when the node is scheduled for deletion).
    working_props: Props,
    /// Repository root URL.
    repo_root: String,
    /// Where the node lives in the repository (for a diff against a revision).
    repos_path: String,
}

fn not_versioned(rel: &RelPath) -> SvnError {
    SvnError::new(
        ErrorCode::PathNotFound,
        format!("'{rel}' is not under version control"),
    )
}

/// Reads at most `limit` bytes from `r`; [`Content::TooLarge`] beyond.
fn read_limited(r: impl Read, limit: usize, what: &str) -> EngineResult<Content> {
    let mut out = Vec::new();
    r.take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|e| SvnError::from_io(format!("cannot read {what}"), &e))?;
    Ok(if out.len() > limit {
        Content::TooLarge
    } else {
        Content::Bytes(out)
    })
}

/// Pristine text of `node` (repository-normal form).
fn pristine_text(db: &WcDb, node: &Node, limit: usize) -> EngineResult<Content> {
    let Some(sha1) = node
        .checksum
        .as_deref()
        .and_then(Sha1Digest::from_svn_string)
    else {
        return Ok(Content::Bytes(Vec::new()));
    };
    let file = db.pristine_store().open_text(&sha1)?;
    read_limited(file, limit, "the pristine text")
}

/// Working file of `rel` converted to repository-normal form.
fn working_text(db: &WcDb, rel: &RelPath, limit: usize) -> EngineResult<Content> {
    let abs = svn_client::wc::abs_path(db, rel)?;
    let md = match fs::symlink_metadata(&abs) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Content::Absent),
        Err(e) => return Err(SvnError::from_io(format!("cannot stat '{rel}'"), &e)),
    };
    if !md.file_type().is_file() && !md.file_type().is_symlink() {
        return Ok(Content::Absent); // obstructed by a directory: no file to show
    }
    if usize::try_from(md.len()).unwrap_or(usize::MAX) > limit {
        return Ok(Content::TooLarge);
    }
    let data = working_repo_normal(db, rel)?;
    // Keyword contraction and line ending normalization may change the size a little.
    Ok(if data.len() > limit {
        Content::TooLarge
    } else {
        Content::Bytes(data)
    })
}

/// Reads both local sides of `rel` from the working copy at `root`.
fn read_local(root: &Path, rel: &RelPath, limit: usize) -> EngineResult<LocalSides> {
    let db = wc_read::open_db(root)?;
    let cur = db.read_node(rel)?.ok_or_else(|| not_versioned(rel))?;
    let deleted = cur.presence == Presence::BaseDeleted;
    if !deleted && !cur.presence.is_present() {
        return Err(not_versioned(rel));
    }
    let base = db.read_base(rel)?.filter(|b| b.presence.is_present());
    // Like `svn diff`: a copied node is compared with its copy source, anything else with BASE
    // (a plain addition has none).
    let copy_source = (cur.op_depth > 0 && !deleted && cur.revision.is_some()).then_some(&cur);
    let old = copy_source.or(base.as_ref());
    let is_file = matches!(cur.kind, NodeKind::File | NodeKind::Symlink);
    let base_content = match old {
        Some(n) if is_file => pristine_text(&db, n, limit)?,
        _ => Content::Absent,
    };
    let working = if deleted || !is_file {
        Content::Absent
    } else {
        working_text(&db, rel, limit)?
    };
    let (repo_root, _) = svn_client::wc::repos_identity(&db)?;
    let repos_path = match base.as_ref().and_then(|b| b.repos_relpath.clone()) {
        Some(p) => p,
        None => {
            // Not in BASE (scheduled for addition): where it would be committed to.
            let root_path = db
                .read_base(&RelPath::root())?
                .and_then(|n| n.repos_relpath)
                .unwrap_or_default();
            svn_client::wc::join_rel(&RelPath::new(&root_path)?, rel)?
                .as_str()
                .to_owned()
        }
    };
    Ok(LocalSides {
        kind: cur.kind,
        base: base_content,
        base_props: old.and_then(|n| n.properties.clone()).unwrap_or_default(),
        working,
        working_props: if deleted {
            Vec::new()
        } else {
            db.actual_props(rel)?
        },
        repo_root,
        repos_path,
    })
}

/// Binary MIME type set on the node, if any.
fn binary_mime(props: &Props) -> Option<String> {
    let mime = prop_value(props, "svn:mime-type").and_then(|v| std::str::from_utf8(v).ok())?;
    svn_diff::diff_as_binary_by_mime(Some(mime)).then(|| mime.trim().to_owned())
}

/// Diff without text sides (directories, too large or binary files).
fn sideless(path: &str, prop_changes: Vec<opensvn_ipc::PropChange>) -> FileDiff {
    FileDiff {
        path: path.to_owned(),
        binary: false,
        mime: None,
        left: None,
        right: None,
        hunks: None,
        prop_changes,
        image: None,
        too_large: None,
        too_complex: None,
    }
}

/// Path of `repos_path` (below the repository root) relative to the session URL, so the same
/// code works on root sessions and on sessions anchored at the working copy URL.
fn session_path(session: &dyn RaSession, repos_path: &str) -> EngineResult<RelPath> {
    let root = session.repos_root().to_string();
    let url = session.session_url().to_string();
    let anchor = percent_decode(
        url.strip_prefix(root.trim_end_matches('/'))
            .unwrap_or_default()
            .trim_matches('/'),
    );
    let repos_path = repos_path.trim_matches('/');
    let rest = if anchor.is_empty() {
        Some(repos_path)
    } else if repos_path == anchor {
        Some("")
    } else {
        repos_path
            .strip_prefix(anchor.as_str())
            .and_then(|r| r.strip_prefix('/'))
    };
    let rest = rest.ok_or_else(|| {
        SvnError::new(
            ErrorCode::Internal,
            format!("/{repos_path} is outside the session at {url}"),
        )
    })?;
    RelPath::new(rest)
}

/// Number of revisions in `(base, latest]` that touch `path`, at most [`INCOMING_CAP`].
async fn count_incoming(
    session: &dyn RaSession,
    path: RelPath,
    base: u64,
    latest: u64,
    cancel: &CancelToken,
) -> EngineResult<u32> {
    if latest <= base {
        return Ok(0);
    }
    let req = LogRequest {
        paths: vec![path],
        start: Some(Revnum(latest)),
        end: Some(Revnum(base + 1)),
        limit: Some(INCOMING_CAP),
        discover_changed_paths: false,
        strict_node_history: false,
    };
    let mut count: u32 = 0;
    let outcome = session
        .log(
            &req,
            &mut |_| {
                count = count.saturating_add(1);
                Ok(())
            },
            cancel,
        )
        .await;
    match outcome {
        Ok(()) => Ok(count.min(INCOMING_CAP)),
        // The location is gone at HEAD: every newer revision counts, up to the cap.
        Err(e) if e.code == ErrorCode::PathNotFound => Ok(u32::try_from(latest - base)
            .unwrap_or(u32::MAX)
            .min(INCOMING_CAP)),
        Err(e) => Err(e),
    }
}

impl RealEngine {
    pub(super) async fn do_status(
        &self,
        wc: &WcRef,
        query: StatusQuery,
        ctx: &OpContext,
    ) -> EngineResult<Vec<FileStatus>> {
        ctx.check()?;
        let opts = StatusOptions {
            depth: Depth::Infinity,
            show_unmodified: false,
            include_unversioned: query.include_unversioned,
            include_ignored: query.include_ignored,
        };
        let statuses =
            svn_client::status(&self.client_ctx(ctx), &wc.root, &opts, &ctx.cancel).await?;
        let mut out: Vec<FileStatus> = statuses.iter().filter_map(wc_map::file_status).collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    pub(super) async fn do_diff_wc(
        &self,
        wc: &WcRef,
        path: &str,
        against: Option<Rev>,
        opts: DiffOptions,
        ctx: &OpContext,
    ) -> EngineResult<FileDiff> {
        ctx.check()?;
        let rel = RelPath::new(path)?;
        let limit = if opts.force_large {
            usize::MAX
        } else {
            CAT_LIMIT
        };
        let local = {
            let (root, rel) = (wc.root.clone(), rel.clone());
            wc_read::blocking(move || read_local(&root, &rel, limit)).await?
        };
        let shown = rel.as_str().to_owned();
        let is_dir = local.kind == NodeKind::Dir;
        // The working MIME type decides; a file scheduled for deletion keeps its old one.
        let mime = binary_mime(&local.working_props).or_else(|| binary_mime(&local.base_props));
        let (left, left_label, left_props) = match against {
            None => (
                local.base,
                format!("{shown} (base)"),
                Some(local.base_props),
            ),
            Some(_) if is_dir => {
                return Err(SvnError::new(
                    ErrorCode::WcInvalidOperation,
                    format!("'{shown}' is a directory; only files can be compared with a revision"),
                ));
            }
            Some(r) => {
                let (content, props) = self
                    .repo_side(&local.repo_root, &local.repos_path, r, limit, ctx)
                    .await?;
                (content, format!("{shown}@r{}", r.0), props)
            }
        };
        // Properties of a side that was too large to download are unknown: no property rows
        // rather than wrong ones.
        let prop_changes = left_props
            .map(|p| wc_map::prop_changes(&p, &local.working_props))
            .unwrap_or_default();
        if is_dir {
            return Ok(sideless(&shown, prop_changes));
        }
        if let Some(mime) = mime {
            return Ok(FileDiff {
                binary: true,
                mime: Some(mime),
                ..sideless(&shown, prop_changes)
            });
        }
        if matches!(left, Content::TooLarge) || matches!(local.working, Content::TooLarge) {
            return Ok(FileDiff {
                too_large: Some(true),
                ..sideless(&shown, prop_changes)
            });
        }
        let right = local.working.side(format!("{shown} (working)"));
        let mut diff = file_diff(&shown, left.side(left_label), right, opts, ctx)?;
        diff.prop_changes = prop_changes;
        Ok(diff)
    }

    /// The file at revision `rev` of the repository, with its properties (`None` when they are
    /// unknown because the file was too large to read).
    async fn repo_side(
        &self,
        repo_root: &str,
        repos_path: &str,
        rev: Rev,
        limit: usize,
        ctx: &OpContext,
    ) -> EngineResult<(Content, Option<Props>)> {
        let (session, path) = self
            .locate_repo_path(repo_root, &RelPath::new(repos_path)?, ctx)
            .await?;
        let mut sink = CappedBuf::new(limit);
        let got = session
            .get_file(&path, Some(Revnum(rev.0)), &mut sink, &ctx.cancel)
            .await;
        match got {
            Ok(meta) => Ok((
                Content::Bytes(sink.into_inner()),
                Some(wc_map::props_from_changes(&meta.props)),
            )),
            Err(_) if sink.exceeded() => Ok((Content::TooLarge, None)),
            // The file did not exist at that revision.
            Err(e) if e.code == ErrorCode::PathNotFound => Ok((Content::Absent, Some(Vec::new()))),
            Err(e) => Err(e),
        }
    }

    pub(super) async fn do_status_remote(
        &self,
        wc: &WcRef,
        ctx: &OpContext,
    ) -> EngineResult<RemoteScan> {
        let info = wc_read::wc_info(&wc.root, ctx).await?;
        let url = RepoUrl::parse(&info.url)?;
        let session = self
            .connector
            .connect_with(&url, &ConnectOptions::at_url(ctx), &ctx.cancel)
            .await?;
        // Unmodified nodes too: the repository side of a clean file is what this call is about.
        let opts = StatusOptions {
            depth: Depth::Infinity,
            show_unmodified: true,
            include_unversioned: false,
            include_ignored: false,
        };
        let statuses = svn_client::status_remote(
            &self.client_ctx(ctx),
            session.as_ref(),
            &wc.root,
            &opts,
            &ctx.cancel,
        )
        .await?;
        // Read after the comparison, so no reported remote revision is newer than `latest_rev`.
        let latest = session.youngest_rev(&ctx.cancel).await?;
        let path = session_path(session.as_ref(), &info.path_in_repo)?;
        let incoming = count_incoming(
            session.as_ref(),
            path,
            info.base_rev.0,
            latest.0,
            &ctx.cancel,
        )
        .await?;
        let mut statuses: Vec<FileStatus> = statuses
            .iter()
            .filter(|s| s.remote.is_some())
            .filter_map(wc_map::file_status)
            .collect();
        statuses.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(RemoteScan {
            statuses,
            incoming,
            latest_rev: Rev(latest.0),
        })
    }
}

//! Working copy reads through `svn-client`: status, working copy diff and remote status.
//!
//! * `status` is `svn_client::status` without unmodified nodes, with the configured global
//!   ignores extended by the working copy's local ignore list (`IgnoreScope::Local`). `svn-client` already lists the
//!   unmodified nodes the facade asks for (locked, changelisted, switched), so no second pass is
//!   needed; statuses are sorted by path like the fake engine's.
//! * `diff_wc` compares two sides in repository-normal form: the pristine text (or the copy
//!   source of a copied node) or the file at a revision read from the repository, against the
//!   working file with keywords and line endings normalized as a commit would send it. Text is
//!   shown through the shared `engine::text` rules (binary, display budget), sides above
//!   [`CAT_LIMIT`] are flagged `too_large` unless the caller forces them (at most `limit + 1`
//!   bytes of any file are ever read, whatever its size on disk), and a binary `svn:mime-type`
//!   (of the working node or of the compared side) wins over content sniffing. Property changes
//!   are listed for files and directories; a `too_large` diff carries no text sides and no hunks
//!   (doc 03 `FileDiff.hunks`), only its property rows.
//! * An unversioned file (status lists them, conflict marker files included) is diffed as a
//!   one-sided addition of the bytes on disk, under the same cap and binary rules. The path must
//!   stay inside the working copy: no `..`, no absolute path, no `.svn` component and no symlinked
//!   parent folder; a symlink itself is shown as its `link <target>` text, never followed.
//! * `status_remote` runs `svn_client::status_remote` on a session anchored at the working copy
//!   URL (with the interactivity of the call) and counts the revisions the working copy lacks,
//!   capped at [`INCOMING_CAP`].

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use opensvn_ipc::{FileDiff, FileStatus, Rev};
use svn_client::StatusOptions;
use svn_client::wc::inspect::{prop_value, translate_info};
use svn_ra_api::{LogRequest, RaSession};
use svn_types::{CancelToken, Depth, ErrorCode, NodeKind, RelPath, RepoUrl, Revnum, SvnError};
use svn_wc::db::{Node, Presence, Props, WcDb};
use svn_wc::pristine::Sha1Digest;
use svn_wc::translate::{ContentTranslator, special, translate_stream};

use super::remote::CAT_LIMIT;
use super::text::CappedBuf;
use super::{ConnectOptions, RealEngine, local_util, wc_map, wc_read};
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
    /// Not under version control: a file on disk only (one-sided addition).
    unversioned: bool,
}

fn not_versioned(rel: &RelPath) -> SvnError {
    SvnError::new(
        ErrorCode::PathNotFound,
        format!("'{rel}' is not under version control"),
    )
}

/// Bytes read from a source for a side of at most `limit` bytes: one more, to tell "exactly at
/// the limit" from "over it".
fn read_cap(limit: usize) -> u64 {
    u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1)
}

/// Reads at most `limit` bytes from `r`; [`Content::TooLarge`] beyond.
fn read_limited(r: impl Read, limit: usize, what: &str) -> EngineResult<Content> {
    let mut out = Vec::new();
    r.take(read_cap(limit))
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

/// Caps bytes already in memory (a symlink's `link <target>` text).
fn capped(data: Vec<u8>, limit: usize) -> Content {
    if data.len() > limit {
        Content::TooLarge
    } else {
        Content::Bytes(data)
    }
}

/// What is on disk at `abs`, without following a symlink: `None` when nothing is there or a
/// directory obstructs it (no file to show), else whether it is a symlink.
fn file_on_disk(abs: &Path, rel: &RelPath) -> EngineResult<Option<bool>> {
    match fs::symlink_metadata(abs) {
        Ok(md) if md.file_type().is_symlink() => Ok(Some(true)),
        Ok(md) if md.file_type().is_file() => Ok(Some(false)),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SvnError::from_io(format!("cannot stat '{rel}'"), &e)),
    }
}

/// Working file of `rel` converted to repository-normal form. The file is streamed through the
/// translator and at most `limit + 1` bytes are read, so a file that grows after it was listed
/// (a log being written) cannot push the read past the cap.
fn working_text(db: &WcDb, rel: &RelPath, limit: usize) -> EngineResult<Content> {
    let abs = svn_client::wc::abs_path(db, rel)?;
    let Some(is_link) = file_on_disk(&abs, rel)? else {
        return Ok(Content::Absent);
    };
    let info = translate_info(&db.actual_props(rel)?);
    if is_link {
        return Ok(capped(special::read_repo_normal(&abs)?, limit));
    }
    let file =
        File::open(&abs).map_err(|e| SvnError::from_io(format!("cannot open '{rel}'"), &e))?;
    if info.special {
        // `svn:special` on a regular file: its raw content, like `special::read_repo_normal`.
        return read_limited(file, limit, "the working file");
    }
    let mut src = file.take(read_cap(limit));
    let mut out = Vec::new();
    translate_stream(
        &mut src,
        &mut out,
        ContentTranslator::to_repo_normal(&info, true)?,
    )?;
    // The source hit the cap, or keyword/eol normalization grew the text past it.
    Ok(if src.limit() == 0 || out.len() > limit {
        Content::TooLarge
    } else {
        Content::Bytes(out)
    })
}

/// Checks that an unversioned `rel` stays inside the working copy at `wcroot`: never inside the
/// administrative area, never through a symlinked parent folder (which could lead outside).
fn unversioned_path(wcroot: &Path, rel: &RelPath) -> EngineResult<std::path::PathBuf> {
    let parts: Vec<&str> = rel.as_str().split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() || parts.iter().any(|p| p.eq_ignore_ascii_case(".svn")) {
        return Err(not_versioned(rel));
    }
    let mut dir = wcroot.to_path_buf();
    for part in &parts[..parts.len() - 1] {
        dir.push(part);
        match fs::symlink_metadata(&dir) {
            Ok(md) if md.file_type().is_dir() => {}
            Ok(_) => {
                return Err(SvnError::new(
                    ErrorCode::WcInvalidOperation,
                    format!("'{rel}' is reached through a link or a file, not a folder"),
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(not_versioned(rel)),
            Err(e) => return Err(SvnError::from_io(format!("cannot stat '{rel}'"), &e)),
        }
    }
    Ok(dir.join(parts[parts.len() - 1]))
}

/// One-sided "added" view of an unversioned path: the raw bytes on disk (a symlink as its
/// `link <target>` text), or an empty directory diff for a folder.
fn read_unversioned(db: &WcDb, rel: &RelPath, limit: usize) -> EngineResult<LocalSides> {
    let abs = unversioned_path(db.wcroot(), rel)?;
    let md = match fs::symlink_metadata(&abs) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(not_versioned(rel)),
        Err(e) => return Err(SvnError::from_io(format!("cannot stat '{rel}'"), &e)),
    };
    let (kind, working) = if md.file_type().is_symlink() {
        (
            NodeKind::File,
            capped(special::read_repo_normal(&abs)?, limit),
        )
    } else if md.file_type().is_file() {
        let file =
            File::open(&abs).map_err(|e| SvnError::from_io(format!("cannot open '{rel}'"), &e))?;
        (NodeKind::File, read_limited(file, limit, "the file")?)
    } else if md.file_type().is_dir() {
        (NodeKind::Dir, Content::Absent)
    } else {
        return Err(not_versioned(rel));
    };
    Ok(LocalSides {
        kind,
        base: Content::Absent,
        base_props: Vec::new(),
        working,
        working_props: Vec::new(),
        repo_root: String::new(),
        repos_path: String::new(),
        unversioned: true,
    })
}

/// Reads both local sides of `rel` from the working copy at `root`.
fn read_local(root: &Path, rel: &RelPath, limit: usize) -> EngineResult<LocalSides> {
    let db = wc_read::open_db(root)?;
    // Not in wc.db, or only a not-present/excluded row: whatever is on disk is unversioned.
    let cur = match db.read_node(rel)? {
        Some(n) if n.presence == Presence::BaseDeleted || n.presence.is_present() => n,
        _ => return read_unversioned(&db, rel, limit),
    };
    let deleted = cur.presence == Presence::BaseDeleted;
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
        unversioned: false,
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
        // Global ignores of the scan: the configured ones (or svn's defaults) plus the working
        // copy's local-scope ignore list, as the local operations use them.
        let mut client = self.client_ctx(ctx);
        let (config, root) = (self.svn_config.clone(), wc.root.clone());
        let globals = wc_read::blocking(move || {
            let mut globals = config.add_config().global_ignores;
            for p in local_util::read_local_ignores(&root)? {
                if !globals.contains(&p) {
                    globals.push(p);
                }
            }
            Ok(globals)
        })
        .await?;
        client.config.global_ignores = Some(globals);
        let opts = StatusOptions {
            depth: Depth::Infinity,
            show_unmodified: false,
            include_unversioned: query.include_unversioned,
            include_ignored: query.include_ignored,
        };
        let statuses = svn_client::status(&client, &wc.root, &opts, &ctx.cancel).await?;
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
        if local.unversioned && against.is_some() {
            return Err(not_versioned(&rel));
        }
        let (left, left_label, left_props) = match against {
            None => (
                local.base,
                format!("{shown} (base)"),
                Some(local.base_props.clone()),
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
        // Binary when either side's MIME type says so: the working one first (a file scheduled
        // for deletion has none), then the compared side's (BASE, or the revision read from the
        // repository; BASE when that revision's properties are unknown).
        let mime = binary_mime(&local.working_props)
            .or_else(|| binary_mime(left_props.as_ref().unwrap_or(&local.base_props)));
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

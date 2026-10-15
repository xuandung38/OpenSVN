//! File externals: a BASE file of the defining working copy marked `file_external`, fetched
//! with `get_file` and applied through the regular update editor, so local modifications are
//! merged (or conflict) exactly like an update of any other file.
//!
//! svn only supports file externals from the working copy's own repository and below a
//! versioned directory of the same working copy; so does this client.

use std::io::Read;
use std::sync::{Arc, PoisonError};

use bytes::Bytes;
use sha1::{Digest, Sha1};
use svn_ra_api::RaSession;
use svn_types::{
    Checksum, DeltaEditor, Depth, DirEntry, ErrorCode, NodeKind, PropChange, RelPath, RepoUrl,
    Result, Revnum, SvnError, TxDeltaWindow,
};
use svn_wc::db::{NewExternal, Node, WcDb};
use svn_wc::pristine::Sha1Digest;

use super::{DirLock, ExternalAction, Item, Outcome, blocking, in_the_way, report};
use crate::ctx::ClientCtx;
use crate::update::{UpdateEditor, UpdateSetup, format_svn_date, repos_relpath_of, root_string};
use crate::wc;

/// Size of the windows the downloaded text is fed to the editor in.
const WINDOW: usize = 64 * 1024;

fn not_in_repository(url: &RepoUrl, root: &str) -> SvnError {
    SvnError::new(
        ErrorCode::WcInvalidOperation,
        format!("unsupported external: URL of file external '{url}' is not in repository '{root}'"),
    )
    .with_svn_code(155_000)
}

/// `url` split into its parent directory URL and its (decoded, validated) last segment.
fn split_url(url: &RepoUrl) -> Result<(RepoUrl, RelPath)> {
    let text = url.to_string();
    let parent = text
        .rsplit_once('/')
        .map(|(p, _)| p)
        .ok_or_else(|| in_the_way(format!("'{url}' does not name a file")))?;
    let parent = RepoUrl::parse(parent)?;
    let name = repos_relpath_of(&parent, url)?;
    Ok((parent, RelPath::from_server(&name)?))
}

/// The working file's repository state before the fetch: what the editor is told to change.
struct Fetched {
    rev: Revnum,
    props: Vec<PropChange>,
    md5: Checksum,
    sha1: String,
    entry: DirEntry,
    text: tempfile::TempPath,
}

/// Fetches the file external of `job` at `url`@`rev` and applies it.
pub(super) async fn ensure(
    job: &Item<'_>,
    url: &RepoUrl,
    rev: Revnum,
    out: &mut Outcome,
) -> Result<()> {
    let db = Arc::clone(job.db);
    let (wc_root, wc_uuid) = {
        let db = Arc::clone(&db);
        blocking(move || wc::repos_identity(&db)).await?
    };
    let root_url = RepoUrl::parse(&wc_root)?;
    // Same scheme, host and port, and below the repository root.
    let (r, u) = (root_url.as_url(), url.as_url());
    if r.scheme() != u.scheme()
        || r.host_str() != u.host_str()
        || r.port_or_known_default() != u.port_or_known_default()
    {
        return Err(not_in_repository(url, &wc_root));
    }
    let wanted = match repos_relpath_of(&root_url, url) {
        Ok(p) if !p.is_empty() => p,
        _ => return Err(not_in_repository(url, &wc_root)),
    };
    let (parent_url, name) = split_url(url)?;
    let session = job.opener.open(&parent_url, job.cancel).await?;
    if session.uuid() != wc_uuid || root_string(session.as_ref()) != wc_root.trim_end_matches('/') {
        return Err(not_in_repository(url, &wc_root));
    }
    let fetched = fetch(session.as_ref(), &db, &name, rev, job).await?;
    let reg = NewExternal {
        kind: NodeKind::File,
        repos_root: wc_root.trim_end_matches('/').to_owned(),
        repos_uuid: wc_uuid.clone(),
        def_local_relpath: job.def_dir.clone(),
        def_repos_relpath: wanted.clone(),
        def_operational_revision: job.item.peg.number(),
        def_revision: job.item.revision.number(),
    };
    let (ctx, local, def, cancel) = (
        job.ctx.clone(),
        job.local.clone(),
        job.def_dir.clone(),
        job.cancel.clone(),
    );
    let (existed, events, revision) = blocking(move || {
        apply(
            &ctx,
            &db,
            &def,
            &local,
            ApplyParams {
                wanted,
                repos_root: wc_root.trim_end_matches('/').to_owned(),
                uuid: wc_uuid,
                reg,
                fetched,
            },
            &cancel,
        )
    })
    .await?;
    let action = if existed {
        ExternalAction::Updated
    } else {
        ExternalAction::Fetched
    };
    out.reports.push(report(
        job.local,
        NodeKind::File,
        action,
        Some(Revnum(revision)),
    ));
    out.events.extend(events);
    Ok(())
}

async fn fetch(
    session: &dyn RaSession,
    db: &Arc<WcDb>,
    name: &RelPath,
    rev: Revnum,
    job: &Item<'_>,
) -> Result<Fetched> {
    let entry = session
        .stat(name, Some(rev), job.cancel)
        .await?
        .filter(|e| e.kind == NodeKind::File)
        .ok_or_else(|| {
            SvnError::new(
                ErrorCode::PathNotFound,
                format!("'{name}' at revision {rev} is not a file"),
            )
        })?;
    let dir = crate::update::tmp_dir_of(db);
    std::fs::create_dir_all(&dir)
        .map_err(|e| SvnError::from_io(format!("cannot create '{}'", dir.display()), &e))?;
    let tmp = tempfile::Builder::new()
        .prefix("external-")
        .tempfile_in(&dir)
        .map_err(|e| SvnError::from_io("cannot create a temporary file", &e))?;
    let std_file = tmp
        .as_file()
        .try_clone()
        .map_err(|e| SvnError::from_io("cannot duplicate temp file handle", &e))?;
    let mut sink = tokio::fs::File::from_std(std_file);
    let meta = session
        .get_file(name, Some(rev), &mut sink, job.cancel)
        .await?;
    drop(sink);
    let text = tmp.into_temp_path();
    let sha1 = {
        let path = text.to_path_buf();
        blocking(move || {
            let mut f = std::fs::File::open(&path)
                .map_err(|e| SvnError::from_io("cannot read the fetched file", &e))?;
            let mut h = Sha1::new();
            let mut buf = vec![0u8; WINDOW];
            loop {
                let n = f
                    .read(&mut buf)
                    .map_err(|e| SvnError::from_io("cannot read the fetched file", &e))?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            let digest: [u8; 20] = h.finalize().into();
            Ok(Sha1Digest(digest).to_svn_string())
        })
        .await?
    };
    Ok(Fetched {
        rev: meta.rev,
        props: meta.props,
        md5: meta.md5,
        sha1,
        entry,
        text,
    })
}

struct ApplyParams {
    wanted: String,
    repos_root: String,
    uuid: String,
    reg: NewExternal,
    fetched: Fetched,
}

/// Property changes that turn `old` (pristine) into `new`.
fn prop_changes(old: &svn_wc::db::Props, new: &[PropChange]) -> Vec<PropChange> {
    let mut out = Vec::new();
    for p in new {
        let value = p.value.as_deref();
        if crate::wc::inspect::prop_value(old, &p.name) != value {
            out.push(p.clone());
        }
    }
    for (k, _) in old {
        let name = String::from_utf8_lossy(k);
        if !new.iter().any(|p| p.name == name) {
            out.push(PropChange {
                name: name.into_owned(),
                value: None,
            });
        }
    }
    out
}

fn entry_props(e: &DirEntry) -> Vec<PropChange> {
    let mut out = Vec::new();
    let mut push = |name: &str, v: Option<String>| {
        if let Some(v) = v {
            out.push(PropChange {
                name: name.to_owned(),
                value: Some(Bytes::from(v)),
            });
        }
    };
    push(
        "svn:entry:committed-rev",
        e.created_rev.map(|r| r.0.to_string()),
    );
    push(
        "svn:entry:committed-date",
        e.time.map(|t| format_svn_date(t.0)),
    );
    push("svn:entry:last-author", e.last_author.clone());
    out
}

/// True when the file external at `base` already is what was fetched.
fn up_to_date(base: &Node, p: &ApplyParams) -> bool {
    let f = &p.fetched;
    base.repos_relpath.as_deref() == Some(p.wanted.as_str())
        && base.revision == Some(f.rev.0)
        && base.checksum.as_deref() == Some(f.sha1.as_str())
        && prop_changes(&base.properties.clone().unwrap_or_default(), &f.props).is_empty()
}

/// Applies the fetched file to the working copy under the lock of its parent directory.
/// Returns whether the external existed before, the notifications and its revision.
fn apply(
    ctx: &ClientCtx,
    db: &Arc<WcDb>,
    def_dir: &RelPath,
    local: &RelPath,
    p: ApplyParams,
    cancel: &svn_types::CancelToken,
) -> Result<(bool, Vec<crate::ctx::UpdateEvent>, u64)> {
    let anchor = local.parent().unwrap_or_else(RelPath::root);
    let name = RelPath::new(local.file_name().unwrap_or_default())?;
    // The parent must be a present directory of this working copy with a BASE row.
    let cannot_insert = || {
        in_the_way(format!(
            "cannot insert a file external defined on '{def_dir}' into the working copy: \
             '{anchor}' is not a versioned directory of it"
        ))
    };
    let parent_base = db.read_base(&anchor)?.ok_or_else(cannot_insert)?;
    let parent_cur = db.read_node(&anchor)?.ok_or_else(cannot_insert)?;
    if parent_base.kind != NodeKind::Dir
        || !parent_base.presence.is_present()
        || !parent_cur.presence.is_present()
    {
        return Err(cannot_insert());
    }
    let _lock = DirLock::obtain(db, &anchor, cancel)?;
    let existing = db.read_base(local)?.filter(|b| b.presence.is_present());
    if let Some(b) = &existing
        && !b.file_external
    {
        return Err(in_the_way(format!(
            "the file external cannot overwrite the existing versioned item at '{local}'"
        )));
    }
    if existing.is_none() && wc::abs_path(db, local)?.symlink_metadata().is_ok() {
        return Err(in_the_way(format!(
            "cannot fetch the file external '{local}': an unversioned item is in the way"
        )));
    }
    let rev = p.fetched.rev.0;
    if let Some(b) = &existing
        && up_to_date(b, &p)
    {
        db.with_txn(|t| t.external_add(local, &p.reg))?;
        return Ok((true, Vec::new(), rev));
    }
    let setup = UpdateSetup {
        db: Arc::clone(db),
        anchor: anchor.clone(),
        target: name.clone(),
        switch_relpath: Some(p.wanted.clone()),
        depth: Depth::Infinity,
        sticky_depth: false,
        allow_unversioned_obstructions: false,
        clean: false,
        repos_root: p.repos_root.clone(),
        uuid: p.uuid.clone(),
        ctx: ctx.clone(),
        cancel: cancel.clone(),
        file_external: Some(p.reg.clone()),
    };
    let (mut ed, outcome) = UpdateEditor::new(setup)?;
    if let Err(e) = drive(&mut ed, &name, existing.as_ref(), &p.fetched) {
        let _ = ed.abort_edit();
        let _ = wc::run_work_queue(db, &svn_types::CancelToken::new());
        return Err(e);
    }
    let events = outcome
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .events
        .clone();
    Ok((existing.is_some(), events, rev))
}

/// Feeds the fetched file to the editor as the drive of a one-file switch.
fn drive(
    ed: &mut UpdateEditor,
    name: &RelPath,
    existing: Option<&Node>,
    f: &Fetched,
) -> Result<()> {
    ed.set_target_revision(f.rev)?;
    let root = ed.open_root(None)?;
    let file = if existing.is_some() {
        ed.open_file(name, root, None)?
    } else {
        ed.add_file(name, root, None)?
    };
    if existing.and_then(|b| b.checksum.as_deref()) != Some(f.sha1.as_str()) {
        let mut sink = ed.apply_textdelta(file, None)?;
        let mut src = std::fs::File::open(&f.text)
            .map_err(|e| SvnError::from_io("cannot read the fetched file", &e))?;
        let mut buf = vec![0u8; WINDOW];
        loop {
            let n = src
                .read(&mut buf)
                .map_err(|e| SvnError::from_io("cannot read the fetched file", &e))?;
            if n == 0 {
                break;
            }
            sink.window(Some(&TxDeltaWindow::full_text(Bytes::copy_from_slice(
                &buf[..n],
            ))))?;
        }
        sink.window(None)?;
    }
    let old = existing
        .and_then(|b| b.properties.clone())
        .unwrap_or_default();
    for change in prop_changes(&old, &f.props)
        .into_iter()
        .chain(entry_props(&f.entry))
    {
        ed.change_file_prop(file, change)?;
    }
    ed.close_file(file, Some(f.md5))?;
    ed.close_directory(root)?;
    ed.close_edit()
}

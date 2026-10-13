//! `S:update-report` request: options plus the working copy's reporter calls.

use svn_types::{Depth, ErrorCode, RelPath, Result, Revnum, SvnError};

use crate::caps::{UrlBuilder, advertise_caps};
use crate::http::{HttpRequest, Method};
use crate::xml::XmlWriter;

/// How file contents travel in the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateMode {
    /// `send-all="true"`: properties and text deltas inline.
    SendAll,
    /// No `send-all`: the answer is a skeleton and file contents are fetched with GET.
    Skelta {
        /// Ask for properties inline (`S:include-props`); needs the `inline-props` capability.
        include_props: bool,
    },
}

/// Parameters of an update, switch, status -u or diff-style report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRequestOptions {
    /// Content delivery mode.
    pub mode: UpdateMode,
    /// Repository path of the update source (`S:src-path`; for a switch, the current location).
    pub src: RelPath,
    /// Revision to update to; `None` means HEAD (the server decides).
    pub target_revision: Option<Revnum>,
    /// Switch destination (`S:dst-path`, sent as an absolute URL).
    pub dst: Option<RelPath>,
    /// Single child of `src` to update (`S:update-target`).
    pub update_target: Option<String>,
    /// Requested depth; `Depth::Unknown` keeps the depth recorded per entry.
    pub depth: Depth,
    /// `S:ignore-ancestry`.
    pub ignore_ancestry: bool,
    /// `S:send-copyfrom-args`.
    pub send_copyfrom_args: bool,
    /// `false` sends `S:text-deltas no` (status -u): the server may still send deltas.
    pub text_deltas: bool,
}

impl UpdateRequestOptions {
    /// Options for a plain update of `src` to HEAD with the given mode.
    pub fn new(mode: UpdateMode, src: RelPath) -> Self {
        Self {
            mode,
            src,
            target_revision: None,
            dst: None,
            update_target: None,
            depth: Depth::Unknown,
            ignore_ancestry: false,
            send_copyfrom_args: false,
            text_deltas: true,
        }
    }
}

struct Entry {
    path: RelPath,
    kind: EntryKind,
}

enum EntryKind {
    Present {
        rev: Revnum,
        depth: Depth,
        start_empty: bool,
        lock_token: Option<String>,
        link: Option<RelPath>,
    },
    Missing,
}

/// Collects the reporter calls of the working copy and renders the request.
pub struct UpdateReportBuilder {
    opts: UpdateRequestOptions,
    entries: Vec<Entry>,
}

fn depth_word(depth: Depth) -> &'static str {
    match depth {
        Depth::Empty => "empty",
        Depth::Files => "files",
        Depth::Immediates => "immediates",
        Depth::Infinity => "infinity",
        Depth::Exclude => "exclude",
        Depth::Unknown => "unknown",
    }
}

impl UpdateReportBuilder {
    /// Starts a report.
    pub fn new(opts: UpdateRequestOptions) -> Self {
        Self {
            opts,
            entries: Vec::new(),
        }
    }

    /// `svn_ra_reporter::set_path`: the working copy has `path` at `rev`.
    pub fn set_path(
        &mut self,
        path: &RelPath,
        rev: Revnum,
        depth: Depth,
        start_empty: bool,
        lock_token: Option<&str>,
    ) {
        self.entries.push(Entry {
            path: path.clone(),
            kind: EntryKind::Present {
                rev,
                depth,
                start_empty,
                lock_token: lock_token.map(str::to_owned),
                link: None,
            },
        });
    }

    /// `svn_ra_reporter::link_path`: `path` is a switched subtree taken from `link` (a path in
    /// the repository) at `rev`.
    pub fn link_path(
        &mut self,
        path: &RelPath,
        link: &RelPath,
        rev: Revnum,
        depth: Depth,
        start_empty: bool,
        lock_token: Option<&str>,
    ) {
        self.entries.push(Entry {
            path: path.clone(),
            kind: EntryKind::Present {
                rev,
                depth,
                start_empty,
                lock_token: lock_token.map(str::to_owned),
                link: Some(link.clone()),
            },
        });
    }

    /// `svn_ra_reporter::delete_path`: the working copy does not have `path`.
    pub fn delete_path(&mut self, path: &RelPath) {
        self.entries.push(Entry {
            path: path.clone(),
            kind: EntryKind::Missing,
        });
    }

    /// Renders `REPORT !svn/me`. The first entry must be the root (`set_path("")`).
    pub fn build(&self, urls: &UrlBuilder) -> Result<HttpRequest> {
        let first_is_root = self.entries.first().is_some_and(|e| {
            e.path.is_root() && matches!(e.kind, EntryKind::Present { link: None, .. })
        });
        if !first_is_root {
            return Err(SvnError::new(
                ErrorCode::Internal,
                "update report must start with set_path for the root",
            ));
        }
        let o = &self.opts;
        let mut w = XmlWriter::new();
        let mut root_attrs = vec![("xmlns:S", "svn:")];
        if o.mode == UpdateMode::SendAll {
            root_attrs.push(("send-all", "true"));
        }
        w.start("S:update-report", &root_attrs);
        if o.mode
            == (UpdateMode::Skelta {
                include_props: true,
            })
        {
            w.element("S:include-props", &[], "yes");
        }
        w.element("S:src-path", &[], urls.public(&o.src)?.path());
        if let Some(rev) = o.target_revision {
            w.element("S:target-revision", &[], &rev.0.to_string());
        }
        if let Some(dst) = &o.dst {
            w.element("S:dst-path", &[], urls.public(dst)?.as_str());
        }
        if let Some(target) = &o.update_target {
            w.element("S:update-target", &[], target);
        }
        if o.ignore_ancestry {
            w.element("S:ignore-ancestry", &[], "yes");
        }
        if o.send_copyfrom_args {
            w.element("S:send-copyfrom-args", &[], "yes");
        }
        if !o.text_deltas {
            w.element("S:text-deltas", &[], "no");
        }
        w.element("S:depth", &[], depth_word(o.depth));
        for e in &self.entries {
            match &e.kind {
                EntryKind::Missing => {
                    w.element("S:missing", &[], e.path.as_str());
                }
                EntryKind::Present {
                    rev,
                    depth,
                    start_empty,
                    lock_token,
                    link,
                } => {
                    let rev = rev.0.to_string();
                    let link = link.as_ref().map(|l| format!("/{l}"));
                    let mut attrs: Vec<(&str, &str)> = vec![("rev", &rev)];
                    if *depth != Depth::Unknown {
                        attrs.push(("depth", depth_word(*depth)));
                    }
                    if let Some(l) = &link {
                        attrs.push(("linkpath", l));
                    }
                    if *start_empty {
                        attrs.push(("start-empty", "true"));
                    }
                    if let Some(t) = lock_token {
                        attrs.push(("lock-token", t));
                    }
                    w.element("S:entry", &attrs, e.path.as_str());
                }
            }
        }
        w.end();
        let req = HttpRequest::new(Method::Report, urls.me()?).xml_body(w.finish()?);
        Ok(advertise_caps(req))
    }
}

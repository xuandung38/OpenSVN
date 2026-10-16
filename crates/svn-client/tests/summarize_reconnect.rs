//! A comparison whose connection fails mid-drive is asked again from the start (nothing is
//! written), like `status -u`; any other failure is reported at once.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
#[path = "support/rec_ra.rs"]
mod rec_ra;

use common::*;
use rec_ra::RecRa;
use svn_client::{SummaryKind, SummaryOptions, summarize_repos};
use svn_types::{CancelToken, ConnectionFailure, ErrorCode, RepoUrl, Revnum, SvnError};

fn broken() -> SvnError {
    SvnError::new(
        ErrorCode::Network,
        "peer closed connection without close_notify",
    )
    .with_connection(ConnectionFailure::TlsClosedUncleanly)
}

/// `trunk` (r1) and `branches/b` (r2) with different texts in every file.
fn seeded() -> svn_ra_api::fake::FakeRaSession {
    let s = session();
    let mut trunk = Vec::new();
    let mut branch = Vec::new();
    for f in 0..4 {
        trunk.push(put(&format!("trunk/f{f}.txt"), &format!("trunk {f}\n")));
        branch.push(put(
            &format!("branches/b/f{f}.txt"),
            &format!("branch {f}\n"),
        ));
    }
    s.seed("alice", "r1", &trunk).expect("seed");
    s.seed("alice", "r2", &branch).expect("seed");
    s
}

fn url(path: &str) -> RepoUrl {
    RepoUrl::parse(&format!("{ROOT_URL}/{path}")).unwrap()
}

#[tokio::test]
async fn a_comparison_cut_mid_drive_is_asked_again() {
    let s = seeded();
    let ra = RecRa::new(s.at(&url("trunk")).unwrap());
    ra.update_breaks
        .lock()
        .unwrap()
        .extend([Some((2, broken())), Some((1, broken())), None]);
    let summary = summarize_repos(
        &ra,
        Revnum(2),
        &url("branches/b"),
        None,
        SummaryOptions::default(),
        &CancelToken::new(),
    )
    .await
    .expect("summary after two broken drives");
    let paths: Vec<(&str, SummaryKind)> = summary
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.kind))
        .collect();
    assert_eq!(
        paths,
        (0..4)
            .map(|f| (
                ["f0.txt", "f1.txt", "f2.txt", "f3.txt"][f],
                SummaryKind::Modified
            ))
            .collect::<Vec<_>>()
    );
    // Three drives: two broken, one complete; nothing kept from the broken ones.
    assert_eq!(ra.update_revisions.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn a_comparison_that_fails_otherwise_is_not_asked_again() {
    let s = seeded();
    let ra = RecRa::new(s.at(&url("trunk")).unwrap());
    ra.update_breaks.lock().unwrap().extend([Some((
        1,
        SvnError::new(ErrorCode::Forbidden, "access denied"),
    ))]);
    let err = summarize_repos(
        &ra,
        Revnum(2),
        &url("branches/b"),
        None,
        SummaryOptions::default(),
        &CancelToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Forbidden);
    assert_eq!(ra.update_revisions.lock().unwrap().len(), 1);
}

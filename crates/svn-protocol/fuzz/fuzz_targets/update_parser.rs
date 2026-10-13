#![no_main]
//! update-report parsing (send-all and skelta) must never panic however the bytes are chunked,
//! every emitted path must be a safe relative path, and the event stream must be a valid editor
//! drive (`DriveChecker`: one root, nothing after it, one delta per file, no children below
//! files, balanced opens and closes).
use libfuzzer_sys::fuzz_target;
use svn_protocol::update::{DriveChecker, UpdateEvent, UpdateParser};
use svn_protocol::{Capabilities, Headers, ResponseHead, ResponseParser, UrlBuilder};
use svn_types::RepoUrl;

fn builder() -> Option<UrlBuilder> {
    let session = RepoUrl::parse("http://svn.test.local/svn/repo").ok()?;
    let headers = Headers::from_pairs([
        ("SVN-Me-Resource", "/svn/repo/!svn/me"),
        ("SVN-Repository-Root", "/svn/repo"),
        ("SVN-Rev-Root-Stub", "/svn/repo/!svn/rvr"),
        ("SVN-Rev-Stub", "/svn/repo/!svn/rev"),
        ("SVN-Txn-Root-Stub", "/svn/repo/!svn/txr"),
        ("SVN-Txn-Stub", "/svn/repo/!svn/txn"),
        ("SVN-VTxn-Root-Stub", "/svn/repo/!svn/vtxr"),
        ("SVN-VTxn-Stub", "/svn/repo/!svn/vtxn"),
    ]);
    let caps = Capabilities::from_options_response(&session, &ResponseHead::new(200, headers), b"")
        .ok()?;
    UrlBuilder::new(&session, &caps).ok()
}

fn path_of(e: &UpdateEvent) -> Option<&svn_types::RelPath> {
    match e {
        UpdateEvent::OpenDir { path, .. }
        | UpdateEvent::AddDir { path, .. }
        | UpdateEvent::OpenFile { path, .. }
        | UpdateEvent::AddFile { path, .. }
        | UpdateEvent::DeleteEntry { path, .. }
        | UpdateEvent::AbsentDir { path }
        | UpdateEvent::AbsentFile { path }
        | UpdateEvent::SetProp { path, .. }
        | UpdateEvent::RemoveProp { path, .. }
        | UpdateEvent::FetchProps { path, .. }
        | UpdateEvent::FetchFile { path, .. }
        | UpdateEvent::TxDeltaStart { path, .. }
        | UpdateEvent::TxDeltaChunk { path, .. }
        | UpdateEvent::TxDeltaEnd { path }
        | UpdateEvent::CloseFile { path, .. }
        | UpdateEvent::CloseDir { path } => Some(path),
        UpdateEvent::ReportStart { .. } | UpdateEvent::TargetRevision(_) => None,
    }
}

fuzz_target!(|data: &[u8]| {
    let Some(urls) = builder() else { return };
    // The first byte chooses the piece size so that splits fall everywhere.
    let step = usize::from(data.first().copied().unwrap_or(0)) % 61 + 1;
    let mut p = UpdateParser::new(urls);
    let mut checker = DriveChecker::new();
    let mut ok = true;
    for part in data.chunks(step) {
        match p.feed(part) {
            Ok(events) => {
                for e in events {
                    if let Some(path) = path_of(&e) {
                        assert!(
                            path.is_root()
                                || svn_types::RelPath::from_server(path.as_str()).is_ok()
                        );
                    }
                    assert!(checker.push(&e).is_ok(), "{e:?}");
                }
            }
            Err(_) => {
                ok = false;
                break;
            }
        }
    }
    if ok && let Ok(events) = p.finish() {
        for e in events {
            assert!(checker.push(&e).is_ok(), "{e:?}");
        }
        assert!(checker.finish().is_ok());
    }
});

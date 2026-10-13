#![no_main]
//! update-report parsing (send-all and skelta) must never panic, however the bytes are chunked,
//! and every path it emits must be a safe relative path.
use libfuzzer_sys::fuzz_target;
use svn_protocol::update::{UpdateEvent, UpdateParser};
use svn_protocol::{Capabilities, Headers, ResponseHead, ResponseParser, UrlBuilder};
use svn_types::RepoUrl;

fn builder() -> Option<UrlBuilder> {
    let session = RepoUrl::parse("http://svn.test.local/svn/repo").ok()?;
    let headers = Headers::from_pairs([
        ("SVN-Me-Resource", "/svn/repo/!svn/me"),
        ("SVN-Repository-Root", "/svn/repo"),
        ("SVN-Rev-Root-Stub", "/svn/repo/!svn/rvr"),
        ("SVN-Rev-Stub", "/svn/repo/!svn/rev"),
    ]);
    let caps = Capabilities::from_options_response(&session, &ResponseHead::new(200, headers), b"")
        .ok()?;
    UrlBuilder::new(&session, &caps).ok()
}

fuzz_target!(|data: &[u8]| {
    let Some(urls) = builder() else { return };
    let split = data.first().map_or(0, |b| usize::from(*b)) % (data.len() + 1);
    let mut p = UpdateParser::new(urls);
    let mut events = Vec::new();
    for part in [&data[..split], &data[split..]] {
        match p.feed(part) {
            Ok(e) => events.extend(e),
            Err(_) => return,
        }
    }
    if let Ok(e) = p.finish() {
        events.extend(e);
    }
    for e in events {
        let path = match &e {
            UpdateEvent::OpenDir { path, .. }
            | UpdateEvent::AddDir { path, .. }
            | UpdateEvent::OpenFile { path, .. }
            | UpdateEvent::AddFile { path, .. }
            | UpdateEvent::DeleteEntry { path }
            | UpdateEvent::SetProp { path, .. } => Some(path),
            _ => None,
        };
        if let Some(path) = path {
            assert!(path.is_root() || svn_types::RelPath::from_server(path.as_str()).is_ok());
        }
    }
});

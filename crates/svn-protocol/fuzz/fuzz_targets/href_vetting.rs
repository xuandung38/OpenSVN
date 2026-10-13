#![no_main]
//! No href, however hostile, may be accepted unless it stays inside the session origin and the
//! repository, and its decoded path is a safe `RelPath`.
use libfuzzer_sys::fuzz_target;
use svn_protocol::{Capabilities, Headers, HrefKind, ResponseHead, UrlBuilder};
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
    ]);
    let caps = Capabilities::from_options_response(&session, &ResponseHead::new(200, headers), b"")
        .ok()?;
    UrlBuilder::new(&session, &caps).ok()
}

fuzz_target!(|data: &[u8]| {
    let Ok(href) = std::str::from_utf8(data) else {
        return;
    };
    let Some(urls) = builder() else { return };
    if let Ok(accepted) = urls.accept_href(href) {
        assert_eq!(accepted.url.host_str(), Some("svn.test.local"));
        assert!(accepted.url.path().starts_with("/svn/repo"));
        let rel = match &accepted.kind {
            HrefKind::RevRoot { path, .. }
            | HrefKind::TxnRoot { path, .. }
            | HrefKind::Public { path } => Some(path),
            _ => None,
        };
        if let Some(rel) = rel {
            assert!(svn_types::RelPath::new(rel.as_str()).is_ok());
            assert!(!rel.components().any(|c| c == ".." || c == ".svn"));
        }
    }
});

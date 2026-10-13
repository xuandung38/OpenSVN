//! curl-based transport for the tests that talk to the compat servers.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use svn_protocol::{Body, Headers, HttpRequest, ResponseHead, error_from_response};

/// The harness writes its test CA below the workspace build directory.
pub fn ca() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../..");
    p.push(["tar", "get"].concat());
    p.push("compat/certs/ca.crt");
    p
}

/// Executes a request with curl and returns status, headers and body.
pub fn send(req: HttpRequest) -> (ResponseHead, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!("svn-protocol-compat-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (hdr, out) = (dir.join("h"), dir.join("b"));
    let mut cmd = Command::new("curl");
    cmd.args(["-sS", "--cacert"])
        .arg(ca())
        .args(["-u", "tester:tester-pass", "-X", req.method.as_str()])
        .arg("-D")
        .arg(&hdr)
        .arg("-o")
        .arg(&out)
        .args(["-H", "Expect:"]);
    for (k, v) in req.headers.iter() {
        cmd.args(["-H", &format!("{k}: {v}")]);
    }
    let body = match req.body {
        Body::Empty => None,
        Body::Bytes(b) => Some(b),
        Body::Stream(_) => panic!("streams are not used here"),
    };
    if body.is_some() {
        cmd.args(["--data-binary", "@-"]);
    }
    cmd.arg(req.url.as_str()).stdin(Stdio::piped());
    let mut child = cmd.spawn().expect("curl");
    if let Some(b) = body {
        child.stdin.take().unwrap().write_all(&b).unwrap();
    } else {
        drop(child.stdin.take());
    }
    assert!(
        child.wait().unwrap().success(),
        "curl failed for {}",
        req.url
    );
    let raw = std::fs::read_to_string(&hdr).unwrap();
    // Several blocks when curl saw an auth challenge first: use the last one.
    let block = raw
        .split(
            "

",
        )
        .filter(|b| !b.trim().is_empty())
        .last()
        .unwrap();
    let mut lines = block.lines();
    let status: u16 = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let mut headers = Headers::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.push(k.trim(), v.trim());
        }
    }
    (
        ResponseHead::new(status, headers),
        std::fs::read(&out).unwrap(),
    )
}

pub fn ok(head: &ResponseHead, body: &[u8], what: &str) {
    assert!(
        (200..300).contains(&head.status),
        "{what}: HTTP {}: {}",
        head.status,
        error_from_response(head.status, &head.headers, body)
    );
}

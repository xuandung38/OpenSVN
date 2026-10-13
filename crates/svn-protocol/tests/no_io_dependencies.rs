//! The protocol layer is sans-IO: its normal dependency tree must hold no runtime or HTTP stack.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;

#[test]
fn normal_dependency_tree_has_no_runtime_or_http_stack() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let out = Command::new(cargo)
        .args([
            "tree",
            "-p",
            "svn-protocol",
            "-e",
            "normal",
            "--prefix",
            "none",
            "--offline",
        ])
        .output()
        .expect("run cargo tree");
    assert!(
        out.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tree = String::from_utf8_lossy(&out.stdout);
    for banned in [
        "tokio",
        "reqwest",
        "hyper",
        "async-std",
        "mio",
        "futures-util",
        "rustls",
        "h2",
    ] {
        assert!(
            !tree
                .lines()
                .any(|l| l.split_whitespace().next() == Some(banned)),
            "svn-protocol depends on {banned}:\n{tree}"
        );
    }
    // Only these workspace crates may be linked in (works for `(/path)` and `(C:\path)` alike).
    for line in tree.lines() {
        let name = line.split_whitespace().next().unwrap_or_default();
        if name.starts_with("svn-") || name == "secure-store" || name == "opensvn" {
            assert!(
                matches!(name, "svn-protocol" | "svn-types"),
                "unexpected workspace dependency {name}"
            );
        }
    }
}

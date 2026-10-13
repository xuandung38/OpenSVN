//! The contract crate must stay free of async runtimes and HTTP stacks.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::process::Command;

#[test]
fn normal_dependency_tree_has_no_runtime_or_http_stack() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let out = Command::new(cargo)
        .args([
            "tree",
            "-p",
            "svn-types",
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
    for banned in ["tokio", "reqwest", "hyper", "async-std", "mio"] {
        assert!(
            !tree
                .lines()
                .any(|l| l.split_whitespace().next() == Some(banned)),
            "svn-types depends on {banned}:\n{tree}"
        );
    }
}

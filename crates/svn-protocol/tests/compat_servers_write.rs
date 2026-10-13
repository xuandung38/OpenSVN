//! Replays the write paths (create-txn, MKCOL, PUT with svndiff, PROPPATCH, COPY, DELETE, LOCK and
//! UNLOCK with the `If:` header, MERGE, abort) against a throwaway repository on the compat
//! servers, then checks with `svnlook` inside the server container that the repository holds
//! exactly what was sent. The repository is created per run and always deleted afterwards; the
//! shared fixture repositories are never touched.
//!
//! Needs `cargo xtask compat up`, `docker` and `curl`; run with
//! `cargo test -p svn-protocol --test compat_servers_write -- --ignored --nocapture --test-threads=1`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod compat_support;

use std::process::Command;

use bytes::Bytes;
use compat_support::{ok, send};
use svn_protocol::commit::{
    LockHandling, LockToken, MergeOptions, PutOptions, abort_txn_request, copy_request,
    create_txn_plan, delete_request, merge_request, mkcol_request, parse_create_txn_response,
    parse_merge_response, proppatch_request, put_request,
};
use svn_protocol::locks::{
    LockOptions, get_lock_request, lock_request, parse_get_lock_response, parse_lock_response,
    unlock_request,
};
use svn_protocol::{Body, Capabilities, TxnName, UrlBuilder, options_request};
use svn_types::{NodeKind, PropChange, RelPath, RepoUrl, Revnum};

fn rel(p: &str) -> RelPath {
    RelPath::new(p).unwrap()
}

/// Big-endian base-128 integer as used by svndiff.
fn varint(mut n: usize) -> Vec<u8> {
    let mut out = vec![(n & 0x7f) as u8];
    n >>= 7;
    while n > 0 {
        out.push((n & 0x7f) as u8 | 0x80);
        n >>= 7;
    }
    out.reverse();
    out
}

/// svndiff version 0 carrying `data` as one "new data" instruction against an empty source.
fn svndiff_full(data: &[u8]) -> Vec<u8> {
    let mut out = b"SVN\0".to_vec();
    if data.is_empty() {
        return out;
    }
    let mut inst = if data.len() < 64 {
        vec![0x80 | data.len() as u8]
    } else {
        let mut v = vec![0x80];
        v.extend(varint(data.len()));
        v
    };
    let mut window = Vec::new();
    for n in [0, 0, data.len(), inst.len(), data.len()] {
        window.extend(varint(n));
    }
    window.append(&mut inst);
    window.extend_from_slice(data);
    out.extend(window);
    out
}

/// A repository created inside the server container for this test only.
struct Throwaway {
    container: String,
    name: String,
}

impl Throwaway {
    fn create(container: &str) -> Self {
        let name = format!("p03-write-{}", std::process::id());
        let me = Self {
            container: container.to_owned(),
            name,
        };
        me.exec(&format!(
            "svnadmin create {p} && chown -R www-data:www-data {p} && chmod -R a+rwX {p}",
            p = me.path()
        ));
        me
    }

    fn path(&self) -> String {
        format!("/var/svn/repos/{}", self.name)
    }

    fn exec(&self, script: &str) -> String {
        let out = Command::new("docker")
            .args(["exec", &self.container, "sh", "-c", script])
            .output()
            .expect("docker");
        assert!(
            out.status.success(),
            "`{script}` failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn look(&self, args: &str) -> String {
        self.exec(&format!("svnlook {args} {}", self.path()))
    }
}

impl Drop for Throwaway {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["exec", &self.container, "rm", "-rf", &self.path()])
            .status();
    }
}

fn run(port: u16, container: &str, label: &str) {
    let repo = Throwaway::create(container);
    let session = RepoUrl::parse(&format!("https://localhost:{port}/svn/{}", repo.name)).unwrap();
    let (h, b) = send(options_request(&session).unwrap());
    let caps = Capabilities::from_options_response(&session, &h, &b).unwrap();
    let urls = UrlBuilder::new(&session, &caps).unwrap();
    let root = RelPath::root();
    let merge = |txn: &TxnName, locks: &[(RelPath, String)]| {
        let opts = MergeOptions {
            locks: LockHandling::Release,
            no_merge_response: true,
            lock_tokens: locks,
        };
        let (h, b) = send(merge_request(&urls, &root, txn, &opts).unwrap());
        ok(&h, &b, "MERGE");
        parse_merge_response(&h, &b).unwrap()
    };
    let begin = |log: &str| -> TxnName {
        let props: [(&str, &[u8]); 1] = [("svn:log", log.as_bytes())];
        let plan = create_txn_plan(&urls, &caps, &props).unwrap();
        assert!(plan.deferred_revprops.is_empty());
        let (h, b) = send(plan.request);
        ok(&h, &b, "create-txn");
        parse_create_txn_response(&h, &b).unwrap()
    };
    let put = |txn: &TxnName, path: &str, data: &[u8], opts: &PutOptions<'_>| {
        let body = Body::Bytes(Bytes::from(svndiff_full(data)));
        let (h, b) = send(put_request(&urls, txn, &rel(path), body, opts).unwrap());
        ok(&h, &b, "PUT");
        h
    };

    // Revision 1: directory, two files (one empty), properties.
    let msg1 = "first commit\nwith <markup> & \u{e9}";
    let txn = begin(msg1);
    let (h, b) = send(mkcol_request(&urls, &txn, &rel("dir")).unwrap());
    ok(&h, &b, "MKCOL");
    let content1 = b"hello\nworld\n";
    let put1 = put(&txn, "dir/hello.txt", content1, &PutOptions::default());
    let md5_1 = put1
        .headers
        .get("X-SVN-Result-Fulltext-MD5")
        .expect("result checksum")
        .to_owned();
    put(&txn, "dir/empty.txt", b"", &PutOptions::default());
    let props = [
        PropChange {
            name: "svn:eol-style".into(),
            value: Some(Bytes::from_static(b"native")),
        },
        PropChange {
            name: "custom:note".into(),
            value: Some(Bytes::from_static(b"x<y & \xc3\xa9")),
        },
    ];
    let (h, b) =
        send(proppatch_request(&urls, &txn, &rel("dir/hello.txt"), &props, None, None).unwrap());
    ok(&h, &b, "PROPPATCH");
    let m1 = merge(&txn, &[]);
    assert_eq!(m1.new_rev, Revnum(1));
    assert_eq!(m1.author.as_deref(), Some("tester"));
    assert_eq!(repo.look("log -r1").trim_end(), msg1);
    assert_eq!(
        repo.exec(&format!("svnlook cat -r1 {} dir/hello.txt", repo.path()))
            .as_bytes(),
        content1
    );
    assert_eq!(
        repo.exec(&format!("svnlook cat -r1 {} dir/empty.txt", repo.path())),
        ""
    );
    assert_eq!(
        repo.exec(&format!(
            "svnlook propget -r1 {} svn:eol-style dir/hello.txt",
            repo.path()
        ))
        .trim(),
        "native"
    );
    assert_eq!(
        repo.exec(&format!(
            "svnlook propget -r1 {} custom:note dir/hello.txt",
            repo.path()
        )),
        "x<y & \u{e9}"
    );
    let changed = repo.look("changed -r1");
    for p in ["A   dir/", "A   dir/hello.txt", "A   dir/empty.txt"] {
        assert!(changed.contains(p), "{changed}");
    }
    println!("{label}: r1 committed and verified");

    // Revision 2: lock, modify with the If header, release the lock through MERGE.
    let path = rel("dir/hello.txt");
    let lk = LockOptions {
        comment: Some("locking <it>".into()),
        current_rev: Some(Revnum(1)),
        ..LockOptions::default()
    };
    let (h, b) = send(lock_request(&urls, &path, &lk).unwrap());
    ok(&h, &b, "LOCK");
    let lock = parse_lock_response(&path, &h, &b).unwrap();
    assert_eq!(lock.owner, "tester");
    assert_eq!(lock.comment.as_deref(), Some("locking <it>"));
    let (h, b) = send(get_lock_request(&urls, &path).unwrap());
    assert_eq!(
        parse_get_lock_response(&path, &h, &b)
            .unwrap()
            .unwrap()
            .token,
        lock.token
    );
    assert!(
        repo.exec(&format!("svnlook lock {} /dir/hello.txt", repo.path()))
            .contains(&lock.token)
    );

    // Without the token the commit is refused (423).
    let stale = begin("must fail");
    let body = Body::Bytes(Bytes::from(svndiff_full(b"nope\n")));
    let (h, _) = send(
        put_request(
            &urls,
            &stale,
            &path,
            body,
            &PutOptions {
                base_rev: Some(Revnum(1)),
                base_md5: Some(&md5_1),
                ..PutOptions::default()
            },
        )
        .unwrap(),
    );
    assert_eq!(h.status, 423, "locked path without token");
    let (h, b) = send(abort_txn_request(&urls, &stale).unwrap());
    ok(&h, &b, "abort");
    assert_eq!(
        repo.exec(&format!("svnadmin lstxns {}", repo.path()))
            .trim(),
        "",
        "aborted transaction is gone"
    );

    let txn = begin("second commit");
    let content2 = b"hello\nworld\nmore\n";
    let opts = PutOptions {
        base_rev: Some(Revnum(1)),
        base_md5: Some(&md5_1),
        lock: Some(LockToken(&lock.token)),
        ..PutOptions::default()
    };
    put(&txn, "dir/hello.txt", content2, &opts);
    let m2 = merge(&txn, &[(rel("dir/hello.txt"), lock.token.clone())]);
    assert_eq!(m2.new_rev, Revnum(2));
    assert_eq!(
        repo.exec(&format!("svnlook cat -r2 {} dir/hello.txt", repo.path()))
            .as_bytes(),
        content2
    );
    assert_eq!(
        repo.exec(&format!("svnlook lock {} /dir/hello.txt", repo.path()))
            .trim(),
        "",
        "release-locks freed it"
    );
    println!("{label}: r2 committed under a lock and verified");

    // Revision 3: copy and delete.
    let txn = begin("third commit");
    let (h, b) = send(
        copy_request(
            &urls,
            Revnum(2),
            &path,
            NodeKind::File,
            &txn,
            &rel("dir/copy.txt"),
        )
        .unwrap(),
    );
    ok(&h, &b, "COPY");
    let (h, b) =
        send(delete_request(&urls, &txn, &rel("dir/empty.txt"), Some(Revnum(2)), None).unwrap());
    ok(&h, &b, "DELETE");
    assert_eq!(merge(&txn, &[]).new_rev, Revnum(3));
    let changed = repo.exec(&format!("svnlook changed -r3 --copy-info {}", repo.path()));
    assert!(
        changed.contains("dir/copy.txt") && changed.contains("dir/hello.txt"),
        "{changed}"
    );
    assert!(changed.contains("D   dir/empty.txt"), "{changed}");
    assert_eq!(
        repo.exec(&format!("svnlook cat -r3 {} dir/copy.txt", repo.path()))
            .as_bytes(),
        content2
    );
    println!("{label}: r3 (copy, delete) committed and verified");

    // Lock and unlock round trip without a commit.
    let (h, b) = send(lock_request(&urls, &path, &LockOptions::default()).unwrap());
    ok(&h, &b, "LOCK");
    let lock = parse_lock_response(&path, &h, &b).unwrap();
    let (h, b) = send(unlock_request(&urls, &path, &lock.token, false).unwrap());
    ok(&h, &b, "UNLOCK");
    let (h, b) = send(get_lock_request(&urls, &path).unwrap());
    assert_eq!(parse_get_lock_response(&path, &h, &b).unwrap(), None);
    assert_eq!(repo.look("youngest").trim(), "3");
}

#[test]
#[ignore = "needs `cargo xtask compat up` (Docker), docker and curl"]
fn write_paths_on_svn_1_14_5() {
    run(8443, "opensvn-compat-svn14-server-1", "1.14.5");
}

#[test]
#[ignore = "needs `cargo xtask compat up` (Docker), docker and curl"]
fn write_paths_on_svn_1_15_0() {
    run(8444, "opensvn-compat-svn15-server-1", "1.15.0");
}

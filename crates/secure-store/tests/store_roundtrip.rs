//! Round-trip behavior for every store implementation plus key-encoding properties.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use proptest::prelude::*;
use secrecy::{ExposeSecret, SecretString};
use secure_store::{CredentialStore, KeyringStore, MemoryStore, account_key};
use svn_types::OriginKey;

fn origin(host: &str, realm: &str) -> OriginKey {
    OriginKey {
        scheme: "https".into(),
        host: host.into(),
        port: 443,
        realm: realm.into(),
    }
}

fn exercise(store: &dyn CredentialStore) {
    let a = origin("svn.test.local", "<https://svn.test.local:443> VisualSVN");
    let b = origin("other.test.local", "<https://svn.test.local:443> VisualSVN");
    assert!(store.get(&a, "alice").expect("get").is_none());
    store
        .set(&a, "alice", &SecretString::from("pw-1"))
        .expect("set");
    store
        .set(&b, "alice", &SecretString::from("pw-2"))
        .expect("set");
    let got = store.get(&a, "alice").expect("get").expect("present");
    assert_eq!(got.expose_secret(), "pw-1");
    // Same realm string on another host is a different entry.
    let got_b = store.get(&b, "alice").expect("get").expect("present");
    assert_eq!(got_b.expose_secret(), "pw-2");
    // Replace.
    store
        .set(&a, "alice", &SecretString::from("pw-3"))
        .expect("replace");
    assert_eq!(
        store
            .get(&a, "alice")
            .expect("get")
            .expect("present")
            .expose_secret(),
        "pw-3"
    );
    assert!(store.delete(&a, "alice").expect("delete"));
    assert!(!store.delete(&a, "alice").expect("delete twice"));
    assert!(store.get(&a, "alice").expect("get").is_none());
    assert!(store.get(&b, "alice").expect("get").is_some());
}

#[test]
fn memory_store_roundtrip() {
    exercise(&MemoryStore::new());
}

#[test]
fn keyring_store_roundtrip_on_mock_backend() {
    let store = KeyringStore::with_store(keyring_core::mock::Store::new().expect("mock store"));
    exercise(&store);
}

/// Hits the real OS store (macOS Keychain / Windows Credential Manager / Secret Service).
/// Run manually: `cargo test -p secure-store -- --ignored os_store`.
#[test]
#[ignore = "touches the real OS credential store"]
fn os_store_roundtrip() {
    let store = KeyringStore::os_default().expect("OS credential store");
    let o = origin("os-store-test.opensvn.invalid", "test realm");
    let user = format!("user-{}", std::process::id());
    store
        .set(&o, &user, &SecretString::from("s3cr3t"))
        .expect("set");
    let got = store.get(&o, &user).expect("get").expect("present");
    assert_eq!(got.expose_secret(), "s3cr3t");
    assert!(store.delete(&o, &user).expect("delete"));
    assert!(store.get(&o, &user).expect("get").is_none());
}

proptest! {
    /// Different (origin, user) pairs never share an account string, whatever separators they contain.
    #[test]
    fn account_key_is_injective(
        h1 in ".{0,8}", r1 in ".{0,8}", u1 in ".{0,8}", p1 in 0u16..4,
        h2 in ".{0,8}", r2 in ".{0,8}", u2 in ".{0,8}", p2 in 0u16..4,
    ) {
        let o1 = OriginKey { scheme: "https".into(), host: h1, port: p1, realm: r1 };
        let o2 = OriginKey { scheme: "https".into(), host: h2, port: p2, realm: r2 };
        let same = o1 == o2 && u1 == u2;
        prop_assert_eq!(same, account_key(&o1, &u1) == account_key(&o2, &u2));
    }
}

#[test]
fn arc_dyn_store_is_usable() {
    let s: Arc<dyn CredentialStore> = Arc::new(MemoryStore::new());
    exercise(s.as_ref());
}

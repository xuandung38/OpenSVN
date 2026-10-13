#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn ok(u: &str) -> bool {
    validate_download_url(&Url::parse(u).unwrap(), RELEASES_REPO).is_ok()
}

#[test]
fn only_the_github_api_of_the_release_repository_is_accepted() {
    assert!(ok(
        "https://api.github.com/repos/xuandung38/OpenSVN-releases/releases/assets/123"
    ));
    for bad in [
        "http://api.github.com/repos/xuandung38/OpenSVN-releases/releases/assets/1",
        "https://github.com/xuandung38/OpenSVN-releases/releases/download/v1/app.tar.gz",
        "https://api.github.com.evil.example/repos/xuandung38/OpenSVN-releases/releases/assets/1",
        "https://evil.example/repos/xuandung38/OpenSVN-releases/releases/assets/1",
        "https://api.github.com/repos/xuandung38/OpenSVN/releases/assets/1",
        "https://api.github.com/repos/xuandung38/OpenSVN-releases-evil/releases/assets/1",
        "https://api.github.com/repos/other/OpenSVN-releases/releases/assets/1",
        "https://api.github.com:8443/repos/xuandung38/OpenSVN-releases/releases/assets/1",
        "https://user:pw@api.github.com/repos/xuandung38/OpenSVN-releases/releases/assets/1",
        "https://api.github.com/repos/xuandung38/OpenSVN-releases/../OpenSVN/releases/assets/1",
        "https://api.github.com/repos/xuandung38/OpenSVN-releases/%2e%2e/x",
        "https://api.github.com/repos/xuandung38/OpenSVN-releases%2f..%2fOpenSVN/x",
        "file:///repos/xuandung38/OpenSVN-releases/x",
        "ftp://api.github.com/repos/xuandung38/OpenSVN-releases/x",
    ] {
        assert!(!ok(bad), "{bad} must be rejected");
    }
}

#[test]
fn memory_store_round_trips_and_clears_without_exposing_the_value_in_debug() {
    let store = MemorySecretStore::default();
    assert!(store.get(TOKEN_KEY).unwrap().is_none());
    store
        .set(TOKEN_KEY, &SecretString::from("ghp_secret".to_owned()))
        .unwrap();
    assert_eq!(
        store.get(TOKEN_KEY).unwrap().unwrap().expose_secret(),
        "ghp_secret"
    );
    assert!(!format!("{:?}", store.get(TOKEN_KEY).unwrap()).contains("ghp_secret"));
    store.delete(TOKEN_KEY).unwrap();
    store.delete(TOKEN_KEY).unwrap();
    assert!(store.get(TOKEN_KEY).unwrap().is_none());
}

#[test]
fn endpoints_must_be_the_github_api_of_the_release_repository() {
    let cfg = |eps: serde_json::Value| serde_json::json!({ "pubkey": "x", "endpoints": eps });
    let good = cfg(serde_json::json!([
        "https://api.github.com/repos/xuandung38/OpenSVN-releases/releases/assets/42"
    ]));
    let eps = configured_endpoints(Some(&good), RELEASES_REPO).unwrap();
    assert_eq!(eps.len(), 1);
    assert_eq!(eps[0].host_str(), Some("api.github.com"));

    for bad in [
        serde_json::json!([
            "https://github.com/xuandung38/OpenSVN-releases/releases/latest/download/latest.json"
        ]),
        serde_json::json!([
            "https://objects.githubusercontent.com/repos/xuandung38/OpenSVN-releases/x"
        ]),
        serde_json::json!([
            "https://api.github.com.evil.example/repos/xuandung38/OpenSVN-releases/x"
        ]),
        serde_json::json!(["http://api.github.com/repos/xuandung38/OpenSVN-releases/x"]),
        serde_json::json!(["https://api.github.com/repos/someone/else/x"]),
        serde_json::json!(["https://api.github.com/repos/xuandung38/OpenSVN-releases/%5c..%5cx"]),
        // one bad endpoint spoils the list: the token would be sent to it
        serde_json::json!([
            "https://api.github.com/repos/xuandung38/OpenSVN-releases/releases/assets/42",
            "https://evil.example/latest.json"
        ]),
        serde_json::json!([]),
        serde_json::json!(["not a url"]),
        serde_json::json!([42]),
    ] {
        let err = configured_endpoints(Some(&cfg(bad.clone())), RELEASES_REPO)
            .expect_err(&format!("{bad} must be refused"));
        assert_eq!(err.code, opensvn_ipc::ErrorCode::InvalidArgument);
    }
    assert!(configured_endpoints(None, RELEASES_REPO).is_err());
    assert!(
        configured_endpoints(Some(&serde_json::json!({ "pubkey": "x" })), RELEASES_REPO).is_err()
    );
}

#[test]
fn keychain_store_keeps_the_token_in_the_os_store_under_an_app_only_origin() {
    let os = Arc::new(secure_store::MemoryStore::new());
    let store = KeychainSecretStore::new(os.clone());
    assert!(store.get(TOKEN_KEY).unwrap().is_none());
    store
        .set(TOKEN_KEY, &SecretString::from("ghp_secret".to_owned()))
        .unwrap();
    // The value lives in the backing credential store, nowhere else.
    use secure_store::CredentialStore;
    let raw = os.get(&app_secret_origin(), TOKEN_KEY).unwrap().unwrap();
    assert_eq!(raw.expose_secret(), "ghp_secret");
    assert_eq!(
        store.get(TOKEN_KEY).unwrap().unwrap().expose_secret(),
        "ghp_secret"
    );
    // No repository origin can collide with it.
    let origin = app_secret_origin();
    assert_eq!(origin.port, 0);
    assert!(origin.realm.starts_with('\u{1}'));
    store.delete(TOKEN_KEY).unwrap();
    store.delete(TOKEN_KEY).unwrap();
    assert!(store.get(TOKEN_KEY).unwrap().is_none());
}

#[test]
fn without_an_os_store_the_token_is_refused_not_kept_elsewhere() {
    let store = UnavailableSecretStore {
        reason: "no Secret Service".into(),
    };
    assert!(
        store
            .set(TOKEN_KEY, &SecretString::from("ghp_secret".to_owned()))
            .is_err()
    );
    assert!(store.get(TOKEN_KEY).unwrap().is_none());
    store.delete(TOKEN_KEY).unwrap();
}

#[test]
fn the_app_wires_the_os_store_and_never_a_memory_store() {
    let lib = include_str!("../lib.rs");
    assert!(lib.contains("secrets: updater::os_secret_store()"));
    assert!(!lib.contains("MemorySecretStore"));
    let me = include_str!("../updater.rs");
    // The memory store only exists in test builds.
    let idx = me.find("pub struct MemorySecretStore").unwrap();
    assert!(me[..idx].trim_end().ends_with("#[derive(Default)]"));
    assert!(me[..idx].contains("#[cfg(test)]\n#[derive(Default)]"));
}

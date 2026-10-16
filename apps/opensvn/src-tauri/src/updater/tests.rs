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

#[test]
fn dangerous_tls_options_disable_updates() {
    let good = "https://api.github.com/repos/xuandung38/OpenSVN-releases/releases/latest";
    for flag in [
        "dangerousAcceptInvalidCerts",
        "dangerousAcceptInvalidHostnames",
        "dangerousInsecureTransportProtocol",
    ] {
        let c = serde_json::json!({ "endpoints": [good], flag: true });
        assert!(
            configured_endpoints(Some(&c), RELEASES_REPO).is_err(),
            "{flag}"
        );
        let off = serde_json::json!({ "endpoints": [good], flag: false });
        assert!(
            configured_endpoints(Some(&off), RELEASES_REPO).is_ok(),
            "{flag}=false"
        );
    }
}

#[test]
fn shipped_config_sets_no_dangerous_updater_option() {
    let text = include_str!("../../tauri.conf.json");
    let json: serde_json::Value = serde_json::from_str(text).unwrap();
    assert!(dangerous_flags(json.pointer("/plugins/updater")).is_empty());
}

#[test]
fn redirects_stay_on_https_github_hosts() {
    let ok = |u: &str, hops| redirect_allowed(&Url::parse(u).unwrap(), hops);
    assert!(ok(
        "https://objects.githubusercontent.com/github-production-release-asset/1",
        1
    ));
    assert!(ok("https://release-assets.githubusercontent.com/x", 1));
    for bad in [
        "http://objects.githubusercontent.com/x",
        "https://objects.githubusercontent.com:8443/x",
        "https://u:p@objects.githubusercontent.com/x",
        "https://evil.example/x",
        "https://objects.githubusercontent.com.evil.example/x",
    ] {
        assert!(!ok(bad, 1), "{bad}");
    }
    assert!(
        !ok("https://api.github.com/x", 5),
        "redirect chains are bounded"
    );
}

#[test]
fn authorization_header_is_sensitive_and_absent_without_a_token() {
    let none = request_headers(None).unwrap();
    assert!(none.is_empty());
    let t = SecretString::from("ghp_secretvalue".to_owned());
    let h = request_headers(Some(&t)).unwrap();
    let auth = h.get(reqwest::header::AUTHORIZATION).unwrap();
    assert!(auth.is_sensitive());
    assert!(!format!("{h:?}").contains("ghp_secretvalue"));
    let bad = SecretString::from("a\nb".to_owned());
    assert!(request_headers(Some(&bad)).is_err());
}

#[test]
fn a_refused_release_request_reads_as_forbidden_so_the_token_can_be_marked() {
    // GitHub answers 401 (bad token) or 404 (private repository the token cannot see); the plugin
    // reports both as `ReleaseNotFound`.
    let e = check_error(tauri_plugin_updater::Error::ReleaseNotFound);
    assert_eq!(e.code, opensvn_ipc::ErrorCode::Forbidden);
    assert!(!e.retryable);
    let other = check_error(tauri_plugin_updater::Error::UnsupportedOs);
    assert_eq!(other.code, opensvn_ipc::ErrorCode::Internal);
}

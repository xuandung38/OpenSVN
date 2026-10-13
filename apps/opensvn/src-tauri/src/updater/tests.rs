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

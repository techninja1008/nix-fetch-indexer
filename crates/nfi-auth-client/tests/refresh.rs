//! Refreshing a cached login, against the mock provider (which also answers
//! the service's auth config).

use nfi_auth_client::{Credentials, StoredToken, TokenStore};
use nfi_test_support::MockIdp;
use serde_json::json;

fn creds(idp: &MockIdp, store: &TokenStore) -> Credentials {
    Credentials::new("t", idp.issuer(), None, "set T_TOKEN").with_store(store.clone())
}

/// An expired login with a refresh token the provider has not seen.
fn plant(idp: &MockIdp, store: &TokenStore, refresh_token: &str) {
    store
        .save_token(&StoredToken {
            issuer: idp.issuer().into(),
            client_id: idp.client_id().into(),
            id_token: idp.token(json!({"sub": "1", "exp": 1000})),
            refresh_token: Some(refresh_token.into()),
        })
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_refresh_uses_the_rotating_token_once() {
    let idp = MockIdp::start("cli").await;
    let d = tempfile::tempdir().unwrap();
    let store = TokenStore::at(d.path());
    plant(&idp, &store, "rt-planted");

    // Two processes' worth of credentials refreshing at once: one refreshes
    // under the lock, the other finds the new token after acquiring it.
    let (a, b) = (creds(&idp, &store), creds(&idp, &store));
    let (ta, tb) = tokio::join!(a.token(false), b.token(false));
    let (ta, tb) = (ta.unwrap().unwrap(), tb.unwrap().unwrap());
    assert_eq!(ta, tb);
    assert_eq!(idp.refreshes(), 1);
    let t = store.token(idp.issuer(), idp.client_id()).unwrap().unwrap();
    assert_eq!(t.id_token, ta);
    assert_eq!(t.refresh_token.as_deref(), Some("rt-refreshed-0"));

    // Later calls reuse the token in memory, even without the cache file.
    store.remove_token(idp.issuer(), idp.client_id()).unwrap();
    assert_eq!(a.token(false).await.unwrap(), Some(ta));
    assert_eq!(idp.refreshes(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_refresh_token_is_removed() {
    let idp = MockIdp::start("cli").await;
    let d = tempfile::tempdir().unwrap();
    let store = TokenStore::at(d.path());
    plant(&idp, &store, "rt-x");
    assert!(creds(&idp, &store).current().await.unwrap().is_some());
    // The provider has rotated it away: no login, and nothing left cached.
    plant(&idp, &store, "rt-x");
    let c = creds(&idp, &store);
    assert_eq!(c.current().await.unwrap(), None);
    assert!(store.token(idp.issuer(), idp.client_id()).unwrap().is_none());
    let e = c.token(false).await.unwrap_err();
    assert!(format!("{e:#}").contains("run `t login` or set T_TOKEN"), "{e:#}");
}

//! Refreshing a cached login, and which URLs it is sent to, against the
//! mock provider (which also answers the service's auth config).

use nfi_auth_client::{Credentials, Identity, StoredToken, TokenStore};
use nfi_test_support::{Impostor, MockIdp};
use serde_json::json;

fn creds(idp: &MockIdp, store: &TokenStore) -> Credentials {
    creds_for(idp.issuer(), store)
}

fn creds_for(url: &str, store: &TokenStore) -> Credentials {
    Credentials::new("t", url, None, "set T_TOKEN").with_store(store.clone())
}

/// An expired login for the provider's URL, with a refresh token the
/// provider has not seen.
fn plant(idp: &MockIdp, store: &TokenStore, refresh_token: &str) {
    store
        .record_url(idp.issuer(), idp.issuer(), idp.client_id())
        .unwrap();
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

#[tokio::test(flavor = "multi_thread")]
async fn other_refresh_errors_keep_the_refresh_token() {
    let idp = MockIdp::start("cli").await;
    let d = tempfile::tempdir().unwrap();
    let store = TokenStore::at(d.path());
    plant(&idp, &store, "rt-y");
    let kept = || {
        store
            .token(idp.issuer(), idp.client_id())
            .unwrap()
            .unwrap()
            .refresh_token
    };

    idp.set_refresh_error(Some("temporarily_unavailable"));
    let e = creds(&idp, &store).token(false).await.unwrap_err();
    assert!(format!("{e:#}").contains("temporarily_unavailable"), "{e:#}");
    assert_eq!(kept().as_deref(), Some("rt-y"));
    idp.set_refresh_error(Some("invalid_client"));
    assert!(creds(&idp, &store).current().await.is_err());
    assert_eq!(kept().as_deref(), Some("rt-y"));
    idp.set_refresh_error(None);

    // The provider unreachable.
    idp.set_down(true);
    assert!(creds(&idp, &store).token(false).await.is_err());
    assert_eq!(kept().as_deref(), Some("rt-y"));
    idp.set_down(false);

    assert!(creds(&idp, &store).token(false).await.unwrap().is_some());
    assert_eq!(idp.refreshes(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn cached_login_is_only_sent_to_urls_logged_in_to() {
    let idp = MockIdp::start("cli").await;
    let evil = Impostor::start(&idp).await;
    let d = tempfile::tempdir().unwrap();
    let store = TokenStore::at(d.path());
    let alice = idp.token(json!({"sub": "1", "preferred_username": "alice"}));
    store
        .save_token(&StoredToken {
            issuer: idp.issuer().into(),
            client_id: idp.client_id().into(),
            id_token: alice.clone(),
            refresh_token: Some("rt-a".into()),
        })
        .unwrap();
    store
        .record_url(&format!("{}/", idp.issuer()), idp.issuer(), idp.client_id())
        .unwrap();
    assert_eq!(
        creds(&idp, &store).token(false).await.unwrap(),
        Some(alice.clone())
    );

    // A URL never logged in to that claims the same provider and client
    // gets nothing: no token, no whoami token, and the hint.
    let c = creds_for(evil.url(), &store);
    let e = c.token(false).await.unwrap_err();
    assert!(format!("{e:#}").contains("run `t login`"), "{e:#}");
    assert_eq!(c.current().await.unwrap(), None);
    assert!(matches!(c.whoami().await.unwrap(), Identity::NotAuthenticated));
    assert!(evil.bearers().is_empty());

    // Logging in there on purpose uses the provider login and records the
    // URL, without a new device login.
    assert_eq!(c.login(false).await.unwrap().username, "mallory");
    assert_eq!(evil.bearers(), std::slice::from_ref(&alice));
    assert_eq!(idp.device_logins(), 0);
    let c = creds_for(evil.url(), &store);
    assert_eq!(c.token(false).await.unwrap(), Some(alice));

    // Logging out of one logs out of all, offline (an unreachable URL works:
    // its record says which login it used), reporting the others.
    let gone = "http://127.0.0.1:9";
    store.record_url(gone, idp.issuer(), idp.client_id()).unwrap();
    let mut others = creds_for(gone, &store).logout().await.unwrap().unwrap();
    others.sort();
    let mut expected = vec![idp.issuer().to_string(), evil.url().to_string()];
    expected.sort();
    assert_eq!(others, expected);
    assert_eq!(creds_for(evil.url(), &store).current().await.unwrap(), None);
    assert!(store.token(idp.issuer(), idp.client_id()).unwrap().is_none());
    assert_eq!(creds(&idp, &store).current().await.unwrap(), None);
    assert_eq!(creds(&idp, &store).logout().await.unwrap(), None);
    assert_eq!(evil.bearers().len(), 1);
}

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
    store.record_url(idp.issuer(), &idp.auth_config()).unwrap();
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
    let e = creds(&idp, &store).current().await.unwrap_err();
    assert!(
        format!("{e:#}").contains("refreshing the login") && format!("{e:#}").contains("invalid_client"),
        "{e:#}"
    );
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
async fn logout_waits_for_a_refresh() {
    let idp = MockIdp::start("cli").await;
    let d = tempfile::tempdir().unwrap();
    let store = TokenStore::at(d.path());
    plant(&idp, &store, "rt-z");
    // The refresh holds the lock while the provider answers (100 ms); a
    // logout meanwhile waits for it, so the refreshed login is not saved
    // back after the logout removed it.
    let (a, b) = (creds(&idp, &store), creds(&idp, &store));
    let (refreshed, logged_out) = tokio::join!(a.token(false), async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        b.logout().await
    });
    assert!(refreshed.unwrap().is_some());
    assert_eq!(logged_out.unwrap(), Some(vec![]));
    assert_eq!(idp.refreshes(), 1);
    assert!(store.token(idp.issuer(), idp.client_id()).unwrap().is_none());
    assert_eq!(creds(&idp, &store).current().await.unwrap(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn login_falls_back_to_the_device_flow() {
    let idp = MockIdp::start("cli").await;
    let evil = Impostor::start(&idp).await;
    let d = tempfile::tempdir().unwrap();
    let store = TokenStore::at(d.path());

    // Refreshing the cached login fails (not `invalid_grant`): a warning,
    // then a device login.
    plant(&idp, &store, "rt-w");
    idp.set_refresh_error(Some("server_error"));
    let c = creds_for(evil.url(), &store);
    assert_eq!(c.login(false).await.unwrap().username, "mallory");
    assert_eq!(idp.device_logins(), 1);
    let t = store.token(idp.issuer(), idp.client_id()).unwrap().unwrap();
    assert_eq!(t.refresh_token.as_deref(), Some("rt-dev-0"));
    assert!(creds_for(evil.url(), &store).current().await.unwrap().is_some());
    idp.set_refresh_error(None);

    // A service rejecting the new login (the provider has no whoami): its
    // record is removed, the provider login kept.
    let e = creds(&idp, &store).login(true).await.unwrap_err();
    assert!(format!("{e:#}").contains("rejected the new login"), "{e:#}");
    assert_eq!(idp.device_logins(), 2);
    assert_eq!(creds(&idp, &store).current().await.unwrap(), None);
    assert!(store.token(idp.issuer(), idp.client_id()).unwrap().is_some());
    assert!(creds_for(evil.url(), &store).current().await.unwrap().is_some());
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
        .record_url(&format!("{}/", idp.issuer()), &idp.auth_config())
        .unwrap();
    assert_eq!(
        creds(&idp, &store).token(false).await.unwrap(),
        Some(alice.clone())
    );

    // A URL never logged in to that claims the same provider and client
    // gets nothing: no token, no whoami token.
    let c = creds_for(evil.url(), &store);
    assert_eq!(c.token(false).await.unwrap(), None);
    assert_eq!(c.current().await.unwrap(), None);
    assert!(matches!(c.whoami().await.unwrap(), Identity::NotAuthenticated));
    assert!(evil.bearers().is_empty());

    // Logging out of it removes nothing: it was not logged in.
    assert_eq!(c.logout().await.unwrap(), None);
    assert!(store.token(idp.issuer(), idp.client_id()).unwrap().is_some());
    assert_eq!(
        creds(&idp, &store).token(false).await.unwrap(),
        Some(alice.clone())
    );

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
    store.record_url(gone, &idp.auth_config()).unwrap();
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

/// The service's settings are only fetched to log in and after a 401: a URL
/// logged in to uses those recorded then, one never logged in to gets no
/// token (until a 401 says it needs one).
#[tokio::test(flavor = "multi_thread")]
async fn settings_are_fetched_only_to_log_in_and_after_a_401() {
    let idp = MockIdp::start("cli").await;
    let evil = Impostor::start(&idp).await;
    let d = tempfile::tempdir().unwrap();
    let store = TokenStore::at(d.path());
    plant(&idp, &store, "rt-s");

    // Recorded: the token (refreshed), whoami, and a hint naming `login`.
    let c = creds(&idp, &store);
    assert!(c.token(false).await.unwrap().is_some());
    assert!(c.current().await.unwrap().is_some());
    assert!(c.hint().contains("run `t login`"), "{}", c.hint());
    // Not recorded: no token, whoami without one, and a generic hint.
    let c = creds_for(evil.url(), &store);
    assert_eq!(c.token(true).await.unwrap(), None);
    assert!(matches!(c.whoami().await.unwrap(), Identity::NotAuthenticated));
    assert_eq!(c.hint(), "authentication required: set T_TOKEN");
    assert_eq!(idp.config_fetches(), 0);
    assert!(evil.bearers().is_empty());

    // After a 401 the settings are fetched (once), which the hint uses.
    // (Logging in again needs a terminal: tests may have one.)
    if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        assert_eq!(c.relogin().await.unwrap(), None);
        assert!(c.hint().contains("run `t login`"), "{}", c.hint());
        assert_eq!(idp.config_fetches(), 1);
    }

    // `login` fetches them, and records them for the URL.
    let c = creds_for(evil.url(), &store);
    assert_eq!(c.login(false).await.unwrap().username, "mallory");
    let fetched = idp.config_fetches();
    let c = creds_for(evil.url(), &store);
    assert!(c.token(false).await.unwrap().is_some());
    assert_eq!(idp.config_fetches(), fetched);
}

/// A record written by an older version (without the settings) is completed
/// from the service once, if it still names the same provider login.
#[tokio::test(flavor = "multi_thread")]
async fn old_records_are_completed_once() {
    let idp = MockIdp::start("cli").await;
    let d = tempfile::tempdir().unwrap();
    let store = TokenStore::at(d.path());
    plant(&idp, &store, "rt-o");
    let urls = d.path().join("urls");
    let f = std::fs::read_dir(&urls).unwrap().next().unwrap().unwrap().path();
    let old = json!({"url": idp.issuer(), "issuer": idp.issuer(), "client_id": idp.client_id()});
    std::fs::write(&f, old.to_string()).unwrap();
    assert!(creds(&idp, &store).token(false).await.unwrap().is_some());
    assert!(creds(&idp, &store).token(false).await.unwrap().is_some());
    assert_eq!(idp.config_fetches(), 1);
    // Naming another provider login: as if never logged in to.
    let other = json!({"url": idp.issuer(), "issuer": idp.issuer(), "client_id": "other"});
    std::fs::write(&f, other.to_string()).unwrap();
    assert_eq!(creds(&idp, &store).token(false).await.unwrap(), None);
}

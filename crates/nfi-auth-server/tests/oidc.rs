use nfi_auth_client::{Credentials, StoredToken, TokenStore};
use nfi_auth_server::{AuthError, Authenticator, Expr, OidcSettings};
use nfi_test_support::MockIdp;
use serde_json::json;

#[tokio::test]
async fn validates_tokens() {
    let idp = MockIdp::start("cli").await;
    let mut s = OidcSettings::new(idp.issuer(), idp.client_id());
    s.session = Some(Expr::new("claims.sid").unwrap());
    let a = Authenticator::new(s).unwrap();
    let p = a
        .authenticate(&idp.token(json!({"sub": "1", "preferred_username": "bob", "sid": "s-9"})))
        .await
        .unwrap();
    assert_eq!(p.username, "bob");
    assert_eq!(p.session.as_deref(), Some("s-9"));
    assert_eq!(p.claims["sub"], "1");

    // Without preferred_username the default falls back to sub.
    let p = a.authenticate(&idp.token(json!({"sub": "42"}))).await.unwrap();
    assert_eq!(p.username, "42");

    for bad in [
        idp.token(json!({"sub": "1", "aud": "someone-else"})),
        idp.token(json!({"sub": "1", "iss": "http://evil"})),
        idp.token(json!({"sub": "1", "exp": 1000})),
        "not-a-jwt".to_string(),
    ] {
        assert!(
            matches!(a.authenticate(&bad).await, Err(AuthError::Invalid(_))),
            "{bad}"
        );
    }
    // A tampered payload fails the signature check.
    let t = idp.token(json!({"sub": "1"}));
    let mut parts: Vec<&str> = t.split('.').collect();
    let other = idp.token(json!({"sub": "2"}));
    parts[1] = other.split('.').nth(1).unwrap();
    assert!(matches!(
        a.authenticate(&parts.join(".")).await,
        Err(AuthError::Invalid(_))
    ));

    // A username expression yielding nothing forbids the token.
    let mut s = OidcSettings::new(idp.issuer(), idp.client_id());
    s.username = Some(Expr::new("has(claims.email) ? claims.email : null").unwrap());
    let a = Authenticator::new(s).unwrap();
    assert!(matches!(
        a.authenticate(&idp.token(json!({"sub": "1"}))).await,
        Err(AuthError::Forbidden(_))
    ));
}

#[tokio::test]
async fn unreachable_provider() {
    let a = Authenticator::new(OidcSettings::new("http://127.0.0.1:1", "c")).unwrap();
    let idp = MockIdp::start("c").await;
    let r = a.authenticate(&idp.token(json!({"sub": "1"}))).await;
    assert!(matches!(r, Err(AuthError::Unavailable(_))), "{r:?}");
}

/// A refreshed login (as the CLIs get it) validates.
#[tokio::test]
async fn refreshed_login_validates() {
    let idp = MockIdp::start("cli").await;
    let a = Authenticator::new(OidcSettings::new(idp.issuer(), idp.client_id())).unwrap();
    let d = tempfile::tempdir().unwrap();
    let store = TokenStore::at(d.path());
    store
        .save_token(&StoredToken {
            issuer: idp.issuer().into(),
            client_id: idp.client_id().into(),
            id_token: idp.token(json!({"sub": "1", "exp": 1000})),
            refresh_token: Some("rt-1".into()),
        })
        .unwrap();
    store
        .record_url(idp.issuer(), idp.issuer(), idp.client_id())
        .unwrap();
    let creds = Credentials::new("t", idp.issuer(), None, "").with_store(store);
    let t = creds.current().await.unwrap().unwrap();
    assert_eq!(idp.refreshes(), 1);
    assert_eq!(a.authenticate(&t).await.unwrap().username, "alice");
}

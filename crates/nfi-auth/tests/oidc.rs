use nfi_auth::client::{Session, TokenStore};
use nfi_auth::server::{AuthError, Authenticator, Expr, OidcSettings};
use nfi_auth::testing::MockIdp;
use serde_json::json;

#[tokio::test]
async fn validates_tokens() {
    let idp = MockIdp::start("cli").await;
    let mut s = idp.settings();
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
    let mut s = idp.settings();
    s.username = Some(Expr::new("has(claims.email) ? claims.email : null").unwrap());
    let a = Authenticator::new(s).unwrap();
    assert!(matches!(
        a.authenticate(&idp.token(json!({"sub": "1"}))).await,
        Err(AuthError::Forbidden(_))
    ));
}

#[tokio::test]
async fn unreachable_provider() {
    let a = Authenticator::new(OidcSettings {
        issuer: "http://127.0.0.1:1".into(),
        client_id: "c".into(),
        audience: None,
        scopes: vec![],
        username: None,
        session: None,
    })
    .unwrap();
    let idp = MockIdp::start("c").await;
    let r = a.authenticate(&idp.token(json!({"sub": "1"}))).await;
    assert!(matches!(r, Err(AuthError::Unavailable(_))), "{r:?}");
}

#[tokio::test]
async fn device_login_and_refresh() {
    let idp = MockIdp::start("cli").await;
    let a = Authenticator::new(idp.settings()).unwrap();
    let d = tempfile::tempdir().unwrap();
    let session = Session::new(TokenStore::at(d.path())).unwrap();
    let cfg = a.config();
    assert_eq!(session.cached_token_for(&cfg).await.unwrap(), None);

    let mut prompted = None;
    let t = session
        .device_login(&cfg, |p| prompted = Some(p.user_code.clone()))
        .await
        .unwrap();
    assert_eq!(prompted.as_deref(), Some("CODE-0"));
    assert_eq!(a.authenticate(&t.id_token).await.unwrap().username, "alice");
    assert_eq!(
        session.cached_token_for(&cfg).await.unwrap(),
        Some(t.id_token.clone())
    );
    assert_eq!(idp.refreshes(), 0);

    // An expired cached token is refreshed transparently.
    idp.set_token_lifetime(-10);
    let t = session.device_login(&cfg, |_| {}).await.unwrap();
    idp.set_token_lifetime(3600);
    idp.set_login_claims(json!({"sub": "u1", "preferred_username": "alice2"}));
    let fresh = session.cached_token_for(&cfg).await.unwrap().unwrap();
    assert_ne!(fresh, t.id_token);
    assert_eq!(idp.refreshes(), 1);
    assert_eq!(a.authenticate(&fresh).await.unwrap().username, "alice2");
    assert_eq!(idp.device_logins(), 2);
}

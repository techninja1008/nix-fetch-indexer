//! The JWKS cache: when keys are fetched, rotation, and an unavailable
//! provider.

use nfi_auth_server::{AuthError, Authenticator, OidcSettings};
use nfi_test_support::MockIdp;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

fn authenticator(idp: &MockIdp) -> Authenticator {
    Authenticator::new(OidcSettings::new(idp.issuer(), idp.client_id())).unwrap()
}

fn claims() -> serde_json::Value {
    json!({"sub": "1", "preferred_username": "alice"})
}

#[tokio::test]
async fn known_keys_are_not_refetched() {
    let idp = MockIdp::start("c").await;
    let a = authenticator(&idp);
    for _ in 0..5 {
        a.authenticate(&idp.token(claims())).await.unwrap();
    }
    // Discovery and JWKS, once.
    assert_eq!(idp.key_fetches(), 2);

    // An unknown key id is rejected; it was just fetched, so no refetch.
    let r = a.authenticate(&idp.token_with_kid(claims(), "other")).await;
    assert!(
        matches!(r, Err(AuthError::Invalid(ref e)) if e.contains("unknown signing key")),
        "{r:?}"
    );
    assert_eq!(r.unwrap_err().status(), 401);
    assert_eq!(idp.key_fetches(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_key_refetch_is_single_flight() {
    let idp = MockIdp::start("c").await;
    let a = Arc::new(authenticator(&idp));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..10 {
        let (a, t) = (a.clone(), idp.token_with_kid(claims(), "other"));
        tasks.spawn(async move { a.authenticate(&t).await });
    }
    while let Some(r) = tasks.join_next().await {
        assert!(matches!(r.unwrap(), Err(AuthError::Invalid(_))));
    }
    assert_eq!(idp.key_fetches(), 2);
}

#[tokio::test]
async fn rotation() {
    let idp = MockIdp::start("c").await;
    let a = authenticator(&idp).with_key_refresh(Duration::ZERO, Duration::from_secs(3600));
    let old = idp.token(claims());
    a.authenticate(&old).await.unwrap();
    idp.rotate_key();
    // The new key id is unknown, so the keys are refetched.
    let new = idp.token(claims());
    assert_eq!(a.authenticate(&new).await.unwrap().username, "alice");
    assert_eq!(idp.key_fetches(), 3);
    // The old key is gone from the new set.
    assert!(matches!(a.authenticate(&old).await, Err(AuthError::Invalid(_))));
}

#[tokio::test]
async fn old_key_sets_are_refreshed_in_the_background() {
    let idp = MockIdp::start("c").await;
    let a = authenticator(&idp).with_key_refresh(Duration::ZERO, Duration::from_millis(200));
    let old = idp.token(claims());
    a.authenticate(&old).await.unwrap();
    assert_eq!(idp.key_fetches(), 2);
    // Revoke the key.
    idp.rotate_key();
    a.authenticate(&old).await.unwrap();
    assert_eq!(idp.key_fetches(), 2, "a fresh set is not refreshed");
    tokio::time::sleep(Duration::from_millis(250)).await;
    // The stale set is still used while it is refreshed.
    a.authenticate(&old).await.unwrap();
    for _ in 0..100 {
        if idp.key_fetches() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(idp.key_fetches(), 3);
    // The new key was picked up by the refresh, without another fetch.
    a.authenticate(&idp.token(claims())).await.unwrap();
    assert_eq!(idp.key_fetches(), 3);
    assert!(matches!(a.authenticate(&old).await, Err(AuthError::Invalid(_))));
}

#[tokio::test]
async fn provider_down() {
    let idp = MockIdp::start("c").await;
    idp.set_down(true);
    let a = authenticator(&idp).with_key_refresh(Duration::from_millis(300), Duration::from_secs(3600));
    for _ in 0..5 {
        let r = a.authenticate(&idp.token(claims())).await;
        assert!(matches!(r, Err(AuthError::Unavailable(_))), "{r:?}");
        assert_eq!(r.unwrap_err().status(), 503);
    }
    // Failed fetches count for the rate limit.
    assert_eq!(idp.key_fetches(), 1);
    idp.set_down(false);
    tokio::time::sleep(Duration::from_millis(350)).await;
    a.authenticate(&idp.token(claims())).await.unwrap();
    assert_eq!(idp.key_fetches(), 3);

    // Keys already fetched keep working while the provider is down.
    idp.set_down(true);
    a.authenticate(&idp.token(claims())).await.unwrap();
}

#[tokio::test]
async fn start() {
    let idp = MockIdp::start("c").await;
    // Unreachable: only a warning; keys are fetched on demand.
    idp.set_down(true);
    let a = Authenticator::start(OidcSettings::new(idp.issuer(), "c"))
        .await
        .unwrap();
    assert_eq!(idp.key_fetches(), 1);
    idp.set_down(false);
    drop(a);
    let a = Authenticator::start(OidcSettings::new(idp.issuer(), "c"))
        .await
        .unwrap();
    assert_eq!(idp.key_fetches(), 3);
    a.authenticate(&idp.token(claims())).await.unwrap();
    assert_eq!(idp.key_fetches(), 3);
    // The provider says it is another issuer.
    let other = idp.issuer().replace("127.0.0.1", "localhost");
    let e = Authenticator::start(OidcSettings::new(&other, "c"))
        .await
        .err()
        .unwrap();
    assert!(e.to_string().contains("issuer mismatch"), "{e}");
}

#[tokio::test]
async fn symmetric_tokens_are_rejected() {
    let idp = MockIdp::start("c").await;
    let a = authenticator(&idp);
    let mut h = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    h.kid = Some("test-key".into());
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let payload = json!({"sub": "1", "iss": idp.issuer(), "aud": "c", "exp": exp});
    let c = jsonwebtoken::encode(&h, &payload, &jsonwebtoken::EncodingKey::from_secret(b"secret")).unwrap();
    let r = a.authenticate(&c).await;
    assert!(matches!(r, Err(AuthError::Invalid(_))), "{r:?}");
}

#[tokio::test]
async fn key_algorithm_and_use_are_checked() {
    let idp = MockIdp::start("c").await;
    let mut jwk = idp.jwk();
    jwk["alg"] = "ES256".into();
    idp.set_jwks(Some(json!({"keys": [jwk]})));
    let a = authenticator(&idp);
    let r = a.authenticate(&idp.token(claims())).await;
    assert!(
        matches!(r, Err(AuthError::Invalid(ref e)) if e.contains("EdDSA")),
        "{r:?}"
    );

    let mut jwk = idp.jwk();
    jwk["use"] = "enc".into();
    idp.set_jwks(Some(json!({"keys": [jwk]})));
    let a = authenticator(&idp);
    let r = a.authenticate(&idp.token(claims())).await;
    assert!(
        matches!(r, Err(AuthError::Invalid(ref e)) if e.contains("unknown signing key")),
        "{r:?}"
    );

    // A key without `alg` or `use` is fine.
    let mut jwk = idp.jwk();
    let o = jwk.as_object_mut().unwrap();
    o.remove("alg");
    o.remove("use");
    idp.set_jwks(Some(json!({"keys": [jwk]})));
    let a = authenticator(&idp);
    a.authenticate(&idp.token(claims())).await.unwrap();
}

//! Guards behind the middleware: status codes, config and whoami responses,
//! and anonymous access.

use axum::routing::get;
use axum::{Extension, Router};
use nfi_auth::{AuthConfig, CONFIG_PATH, WHOAMI_PATH, Whoami};
use nfi_auth_server::{Authenticator, Caller, Expr, Guard, OidcSettings, middleware};
use nfi_test_support::MockIdp;
use serde_json::json;
use std::sync::Arc;

/// Serve `guard` like the services do: `/x` and whoami behind the
/// middleware, config and `/healthz` outside it.
async fn serve(guard: Guard) -> String {
    let guard = Arc::new(guard);
    let (g1, g2) = (guard.clone(), guard.clone());
    let app =
        Router::new()
            .route("/x", get(|| async { "x" }))
            .route(
                WHOAMI_PATH,
                get(move |c: Option<Extension<Caller>>| async move {
                    g1.whoami_response(c.as_ref().map(|c| &c.0))
                }),
            )
            .layer(axum::middleware::from_fn_with_state(guard, middleware))
            .route("/healthz", get(|| async { "ok" }))
            .route(CONFIG_PATH, get(move || async move { g2.config_response() }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

fn oidc(idp: &MockIdp, whoami: Option<&str>, policy: &str, anonymous: bool) -> Guard {
    let mut s = OidcSettings::new(idp.issuer(), idp.client_id());
    s.session = Some(Expr::new("has(claims.sid) ? claims.sid : null").unwrap());
    s.whoami = whoami.map(|e| Expr::new(e).unwrap());
    Guard::Oidc {
        authenticator: Arc::new(Authenticator::new(s).unwrap()),
        policy: Expr::new(policy).unwrap(),
        anonymous,
    }
}

async fn get_(url: String, token: Option<&str>) -> reqwest::Response {
    let r = reqwest::Client::new().get(url);
    match token {
        Some(t) => r.bearer_auth(t),
        None => r,
    }
    .send()
    .await
    .unwrap()
}

async fn error(r: reqwest::Response) -> String {
    r.json::<serde_json::Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn open_and_token() {
    let u = serve(Guard::Open).await;
    assert_eq!(get_(format!("{u}/x"), None).await.status(), 200);
    for p in [CONFIG_PATH, WHOAMI_PATH] {
        let r = get_(format!("{u}{p}"), None).await;
        assert_eq!(r.status(), 404);
        assert_eq!(error(r).await, "this listener does not use OIDC login");
    }

    let u = serve(Guard::Token("s3cret".into())).await;
    let r = get_(format!("{u}/x"), None).await;
    assert_eq!(r.status(), 401);
    assert_eq!(r.headers()["www-authenticate"], "Bearer");
    assert_eq!(error(r).await, "missing bearer token");
    assert_eq!(get_(format!("{u}/x"), Some("wrong")).await.status(), 401);
    assert_eq!(get_(format!("{u}/x"), Some("s3cret")).await.status(), 200);
    assert_eq!(get_(format!("{u}/healthz"), None).await.status(), 200);
    assert_eq!(get_(format!("{u}{CONFIG_PATH}"), None).await.status(), 404);
    assert_eq!(
        get_(format!("{u}{WHOAMI_PATH}"), Some("s3cret")).await.status(),
        404
    );
}

#[tokio::test]
async fn oidc_config_and_policy() {
    let idp = MockIdp::start("c").await;
    let u = serve(oidc(&idp, None, "user.username != 'mallory'", false)).await;
    let cfg: AuthConfig = get_(format!("{u}{CONFIG_PATH}"), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(cfg.client_id, "c");
    assert!(!cfg.anonymous);
    let r = get_(format!("{u}/x"), None).await;
    assert_eq!(r.status(), 401);
    assert_eq!(r.headers()["www-authenticate"], "Bearer");
    let r = get_(format!("{u}{WHOAMI_PATH}"), None).await;
    assert_eq!(r.status(), 401);
    let mallory = idp.token(json!({"sub": "m", "preferred_username": "mallory"}));
    let r = get_(format!("{u}/x"), Some(&mallory)).await;
    assert_eq!(r.status(), 403);
    assert!(r.headers().get("www-authenticate").is_none());
    assert_eq!(error(r).await, "forbidden: denied by policy");
    let alice = idp.token(json!({"sub": "a", "preferred_username": "alice"}));
    assert_eq!(get_(format!("{u}/x"), Some(&alice)).await.status(), 200);

    // Anonymous access is advertised; whoami still needs a login.
    let u = serve(oidc(&idp, None, "false", true)).await;
    let cfg: AuthConfig = get_(format!("{u}{CONFIG_PATH}"), None)
        .await
        .json()
        .await
        .unwrap();
    assert!(cfg.anonymous);
    assert_eq!(get_(format!("{u}/x"), None).await.status(), 200);
    assert_eq!(get_(format!("{u}/x"), Some(&alice)).await.status(), 403);
    let r = get_(format!("{u}{WHOAMI_PATH}"), None).await;
    assert_eq!(r.status(), 401);
    assert_eq!(r.headers()["www-authenticate"], "Bearer");
}

#[tokio::test]
async fn whoami() {
    let idp = MockIdp::start("c").await;
    let t = idp.token(json!({"sub": "1", "preferred_username": "alice", "sid": "s1", "groups": ["dev"]}));

    // Default: the claims.
    let u = serve(oidc(&idp, None, "true", false)).await;
    let me: Whoami = get_(format!("{u}{WHOAMI_PATH}"), Some(&t))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me.username, "alice");
    assert_eq!(me.session.as_deref(), Some("s1"));
    assert_eq!(me.additional_info["groups"], json!(["dev"]));
    assert_eq!(me.additional_info["sub"], "1");
    assert_eq!(me.additional_info["iss"], idp.issuer());

    // A custom expression over `claims` and `user`.
    let u = serve(oidc(
        &idp,
        Some("{'teams': claims.groups, 'name': user.username, 'n': 2}"),
        "true",
        false,
    ))
    .await;
    let me: Whoami = get_(format!("{u}{WHOAMI_PATH}"), Some(&t))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me.username, "alice");
    assert_eq!(
        serde_json::Value::Object(me.additional_info),
        json!({"teams": ["dev"], "name": "alice", "n": 2})
    );

    // Not a map: a server error.
    let u = serve(oidc(&idp, Some("claims.sub"), "true", false)).await;
    let r = get_(format!("{u}{WHOAMI_PATH}"), Some(&t)).await;
    assert_eq!(r.status(), 500);
    assert_eq!(error(r).await, "whoami expression failed");
}

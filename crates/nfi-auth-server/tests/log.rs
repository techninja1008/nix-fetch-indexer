//! The request log of the middleware. In its own test binary: tracing
//! caches per-callsite interest, so tests logging concurrently without a
//! subscriber could hide these lines.

use axum::routing::get;
use axum::{Extension, Router};
use nfi_auth::{CONFIG_PATH, WHOAMI_PATH};
use nfi_auth_server::{Authenticator, Caller, Expr, Guard, OidcSettings, middleware};
use nfi_test_support::MockIdp;
use serde_json::json;
use std::sync::{Arc, Mutex};

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

/// Collects formatted log output.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Log {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Log {
    fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .map(str::to_string)
            .collect()
    }
}

#[tokio::test]
async fn request_log() {
    let log = Log::default();
    let w = log.clone();
    let sub = tracing_subscriber::fmt()
        .with_writer(move || w.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    // The test runtime is single-threaded, so the server logs here too.
    let _g = tracing::subscriber::set_default(sub);

    let idp = MockIdp::start("c").await;
    let u = serve(oidc(
        &idp,
        Some("claims.sub"),
        "user.username != 'mallory'",
        false,
    ))
    .await;
    let alice = idp.token(json!({"sub": "a", "preferred_username": "alice", "sid": "s1"}));
    let mallory = idp.token(json!({"sub": "m", "preferred_username": "mallory"}));
    get_(format!("{u}/x"), Some(&alice)).await;
    get_(format!("{u}/x"), Some(&mallory)).await;
    get_(format!("{u}/x"), None).await;
    get_(format!("{u}{WHOAMI_PATH}"), Some(&alice)).await;
    get_(format!("{u}{CONFIG_PATH}"), None).await;

    let lines = log.lines();
    let find = |pat: &[&str]| {
        lines
            .iter()
            .find(|l| pat.iter().all(|p| l.contains(p)))
            .unwrap_or_else(|| panic!("no line with {pat:?} in {lines:#?}"))
            .clone()
    };
    let l = find(&[
        "request{method=GET path=/x user=alice session=s1}",
        "request status=200",
    ]);
    assert!(l.contains(" INFO "), "{l}");
    let l = find(&["request rejected", "status=403"]);
    assert!(l.contains(" INFO ") && l.contains("method=GET path=/x"), "{l}");
    assert!(l.contains("error=forbidden: denied by policy"), "{l}");
    find(&["request rejected", "status=401", "error=missing bearer token"]);
    let l = find(&["whoami expression failed"]);
    assert!(l.contains(" ERROR ") && l.contains("user=alice"), "{l}");
    find(&["path=/auth/v1/whoami user=alice", "request status=500"]);
    // Config is outside the middleware: not logged.
    assert!(!lines.iter().any(|l| l.contains(CONFIG_PATH)), "{lines:#?}");
}

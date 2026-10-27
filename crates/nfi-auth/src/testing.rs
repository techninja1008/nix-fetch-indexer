//! An in-process mock OpenID provider for tests: discovery, JWKS (one
//! Ed25519 key), and a device authorization grant that approves on the
//! second poll. [`pty`] runs CLIs on a pseudo-terminal, so that their
//! interactive paths (device login, fullscreen UIs) can be tested too.

#[cfg(unix)]
pub mod pty;

use crate::server::OidcSettings;
use axum::Router;
use axum::extract::{Form, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// A fixed test-only key; never trust it outside tests.
const KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIF72m8OXAiDCKpJZHZT5LR+HpEM1hIh/Cz780L7gVQ/p
-----END PRIVATE KEY-----
";
const KEY_X: &str = "1MXjeBTbKUuBzqXMCFEMmf641-FsJ5I2RrLXzH7KSY4";
const KID: &str = "test-key";

struct Inner {
    issuer: String,
    client_id: String,
    /// Claims of tokens issued by the device and refresh grants.
    login_claims: Mutex<serde_json::Value>,
    /// Device code -> number of polls so far.
    pending: Mutex<HashMap<String, usize>>,
    device_logins: AtomicUsize,
    refreshes: AtomicUsize,
    token_lifetime: Mutex<i64>,
}

#[derive(Clone)]
pub struct MockIdp {
    inner: Arc<Inner>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

impl MockIdp {
    /// Start a provider on a free loopback port for `client_id`.
    pub async fn start(client_id: &str) -> Self {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", l.local_addr().unwrap());
        let idp = MockIdp {
            inner: Arc::new(Inner {
                issuer,
                client_id: client_id.into(),
                login_claims: Mutex::new(serde_json::json!({"sub": "u1", "preferred_username": "alice"})),
                pending: Default::default(),
                device_logins: AtomicUsize::new(0),
                refreshes: AtomicUsize::new(0),
                token_lifetime: Mutex::new(3600),
            }),
        };
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/device", post(device))
            .route("/token", post(token))
            .with_state(idp.clone());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        idp
    }

    pub fn issuer(&self) -> &str {
        &self.inner.issuer
    }

    pub fn client_id(&self) -> &str {
        &self.inner.client_id
    }

    /// Settings for a service trusting this provider.
    pub fn settings(&self) -> OidcSettings {
        OidcSettings {
            issuer: self.inner.issuer.clone(),
            client_id: self.inner.client_id.clone(),
            audience: None,
            scopes: vec![],
            username: None,
            session: None,
        }
    }

    /// Sign an ID token with `claims`, filling in `iss`, `aud`, `iat` and
    /// `exp` unless given.
    pub fn token(&self, claims: serde_json::Value) -> String {
        let mut c = claims.as_object().cloned().unwrap_or_default();
        let lifetime = *self.inner.token_lifetime.lock().unwrap();
        c.entry("iss").or_insert(self.inner.issuer.clone().into());
        c.entry("aud").or_insert(self.inner.client_id.clone().into());
        c.entry("iat").or_insert(now().into());
        c.entry("exp").or_insert((now() + lifetime).into());
        let mut h = Header::new(Algorithm::EdDSA);
        h.kid = Some(KID.into());
        let key = EncodingKey::from_ed_pem(KEY_PEM.as_bytes()).unwrap();
        jsonwebtoken::encode(&h, &c, &key).unwrap()
    }

    /// Claims of tokens issued by the device and refresh grants.
    pub fn set_login_claims(&self, claims: serde_json::Value) {
        *self.inner.login_claims.lock().unwrap() = claims;
    }

    /// Lifetime of tokens issued from now on, in seconds (may be negative).
    pub fn set_token_lifetime(&self, secs: i64) {
        *self.inner.token_lifetime.lock().unwrap() = secs;
    }

    pub fn device_logins(&self) -> usize {
        self.inner.device_logins.load(Ordering::SeqCst)
    }

    pub fn refreshes(&self) -> usize {
        self.inner.refreshes.load(Ordering::SeqCst)
    }

    fn login_token(&self) -> String {
        self.token(self.inner.login_claims.lock().unwrap().clone())
    }
}

async fn discovery(State(i): State<MockIdp>) -> Response {
    let b = &i.inner.issuer;
    axum::Json(serde_json::json!({
        "issuer": b,
        "jwks_uri": format!("{b}/jwks"),
        "token_endpoint": format!("{b}/token"),
        "device_authorization_endpoint": format!("{b}/device"),
    }))
    .into_response()
}

async fn jwks() -> Response {
    axum::Json(serde_json::json!({"keys": [{
        "kty": "OKP", "crv": "Ed25519", "x": KEY_X, "kid": KID, "alg": "EdDSA", "use": "sig",
    }]}))
    .into_response()
}

async fn device(State(i): State<MockIdp>, Form(f): Form<HashMap<String, String>>) -> Response {
    if f.get("client_id") != Some(&i.inner.client_id) {
        return (axum::http::StatusCode::BAD_REQUEST, "unknown client").into_response();
    }
    let n = i.inner.device_logins.fetch_add(1, Ordering::SeqCst);
    let code = format!("dev-{n}");
    i.inner.pending.lock().unwrap().insert(code.clone(), 0);
    let b = &i.inner.issuer;
    axum::Json(serde_json::json!({
        "device_code": code,
        "user_code": format!("CODE-{n}"),
        "verification_uri": format!("{b}/activate"),
        "verification_uri_complete": format!("{b}/activate?code=CODE-{n}"),
        "expires_in": 60,
        "interval": 0,
    }))
    .into_response()
}

fn oauth_error(e: &str) -> Response {
    (
        axum::http::StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({"error": e})),
    )
        .into_response()
}

async fn token(State(i): State<MockIdp>, Form(f): Form<HashMap<String, String>>) -> Response {
    if f.get("client_id") != Some(&i.inner.client_id) {
        return oauth_error("invalid_client");
    }
    match f.get("grant_type").map(String::as_str) {
        Some("urn:ietf:params:oauth:grant-type:device_code") => {
            let code = f.get("device_code").cloned().unwrap_or_default();
            let mut p = i.inner.pending.lock().unwrap();
            let Some(polls) = p.get_mut(&code) else {
                return oauth_error("expired_token");
            };
            *polls += 1;
            if *polls < 2 {
                return oauth_error("authorization_pending");
            }
            p.remove(&code);
            drop(p);
            axum::Json(serde_json::json!({
                "id_token": i.login_token(),
                "access_token": "opaque",
                "refresh_token": format!("rt-{code}"),
                "token_type": "Bearer",
            }))
            .into_response()
        }
        Some("refresh_token") if f.get("refresh_token").is_some_and(|t| t.starts_with("rt-")) => {
            i.inner.refreshes.fetch_add(1, Ordering::SeqCst);
            axum::Json(serde_json::json!({
                "id_token": i.login_token(),
                "access_token": "opaque",
                "token_type": "Bearer",
            }))
            .into_response()
        }
        Some("refresh_token") => oauth_error("invalid_grant"),
        _ => oauth_error("unsupported_grant_type"),
    }
}

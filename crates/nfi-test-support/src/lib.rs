//! An in-process mock OpenID provider for tests: discovery, JWKS (one
//! Ed25519 key at a time; [`MockIdp::rotate_key`] switches to a second one),
//! and a device authorization grant that approves on the second poll. [`pty`] runs CLIs on a pseudo-terminal, so that their
//! interactive paths (device login, fullscreen UIs) can be tested too.

#[cfg(unix)]
pub mod pty;

use axum::Router;
use axum::extract::{Form, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Fixed test-only keys (PEM, public `x`, key id); never trust them
/// outside tests.
const KEYS: [(&str, &str, &str); 2] = [
    (
        "-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIF72m8OXAiDCKpJZHZT5LR+HpEM1hIh/Cz780L7gVQ/p
-----END PRIVATE KEY-----
",
        "1MXjeBTbKUuBzqXMCFEMmf641-FsJ5I2RrLXzH7KSY4",
        "test-key",
    ),
    (
        "-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIEiwxMFUKdp4Y46rQl8dPl6p1cfK2r+Y7b+03ybpVXxK
-----END PRIVATE KEY-----
",
        "4_QIx86QKBWM3DSGCNEygYLIjYzT6tXBDgbde4Z_kqk",
        "test-key-2",
    ),
];

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
    /// Index into [`KEYS`] of the signing (and published) key.
    key: AtomicUsize,
    /// Published instead of the current key's JWKS when set.
    jwks: Mutex<Option<serde_json::Value>>,
    /// Discovery and JWKS answer 503 while set.
    down: AtomicBool,
    /// Discovery and JWKS requests served or refused so far.
    key_fetches: AtomicUsize,
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
                key: AtomicUsize::new(0),
                jwks: Mutex::new(None),
                down: AtomicBool::new(false),
                key_fetches: AtomicUsize::new(0),
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

    /// Sign an ID token with `claims`, filling in `iss`, `aud`, `iat` and
    /// `exp` unless given.
    pub fn token(&self, claims: serde_json::Value) -> String {
        let kid = KEYS[self.inner.key.load(Ordering::SeqCst)].2;
        self.token_with_kid(claims, kid)
    }

    /// [`MockIdp::token`] with the current key but `kid` in the header.
    pub fn token_with_kid(&self, claims: serde_json::Value, kid: &str) -> String {
        let mut c = claims.as_object().cloned().unwrap_or_default();
        let lifetime = *self.inner.token_lifetime.lock().unwrap();
        c.entry("iss").or_insert(self.inner.issuer.clone().into());
        c.entry("aud").or_insert(self.inner.client_id.clone().into());
        c.entry("iat").or_insert(now().into());
        c.entry("exp").or_insert((now() + lifetime).into());
        let mut h = Header::new(Algorithm::EdDSA);
        h.kid = Some(kid.into());
        let pem = KEYS[self.inner.key.load(Ordering::SeqCst)].0;
        let key = EncodingKey::from_ed_pem(pem.as_bytes()).unwrap();
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

    /// Sign new tokens with the other key, and publish only that one.
    pub fn rotate_key(&self) {
        self.inner.key.fetch_xor(1, Ordering::SeqCst);
    }

    /// The JWK of the current key.
    pub fn jwk(&self) -> serde_json::Value {
        let (_, x, kid) = KEYS[self.inner.key.load(Ordering::SeqCst)];
        serde_json::json!({
            "kty": "OKP", "crv": "Ed25519", "x": x, "kid": kid, "alg": "EdDSA", "use": "sig",
        })
    }

    /// Publish this JWK set instead of the current key's (`None`: back to it).
    pub fn set_jwks(&self, jwks: Option<serde_json::Value>) {
        *self.inner.jwks.lock().unwrap() = jwks;
    }

    /// While down, discovery and JWKS requests get 503.
    pub fn set_down(&self, down: bool) {
        self.inner.down.store(down, Ordering::SeqCst);
    }

    /// Discovery and JWKS requests received so far (including refused ones).
    pub fn key_fetches(&self) -> usize {
        self.inner.key_fetches.load(Ordering::SeqCst)
    }

    /// Count a discovery or JWKS request; `Some` (a 503) while down.
    fn key_request(&self) -> Option<Response> {
        self.inner.key_fetches.fetch_add(1, Ordering::SeqCst);
        self.inner
            .down
            .load(Ordering::SeqCst)
            .then(|| (axum::http::StatusCode::SERVICE_UNAVAILABLE, "down").into_response())
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
    if let Some(r) = i.key_request() {
        return r;
    }
    let b = &i.inner.issuer;
    axum::Json(serde_json::json!({
        "issuer": b,
        "jwks_uri": format!("{b}/jwks"),
        "token_endpoint": format!("{b}/token"),
        "device_authorization_endpoint": format!("{b}/device"),
    }))
    .into_response()
}

async fn jwks(State(i): State<MockIdp>) -> Response {
    if let Some(r) = i.key_request() {
        return r;
    }
    let set = i.inner.jwks.lock().unwrap().clone();
    axum::Json(set.unwrap_or_else(|| serde_json::json!({"keys": [i.jwk()]}))).into_response()
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

//! Service side: ID token validation, principal extraction and CEL policies.
//!
//! An [`Authenticator`] validates an OIDC ID token against the issuer's JWKS
//! (signature, `iss`, `aud`, `exp`; keys cached as described in [`keys`])
//! and turns its claims into a [`Principal`] with CEL expressions.
//! Authorisation is a CEL expression ([`Expr`]) that must evaluate to
//! `true`; it sees the variables [`Principal::vars`] provides plus any the
//! service adds. A [`Guard`] puts this (or a fixed token, or nothing) in
//! front of a listener with [`middleware`].
//!
//! CEL variables:
//! - `claims`: the token's claims, as a map.
//! - `user`: `{"username": string, "session": string or null}`.
//! - whatever else the service documents, e.g. `request` ([`request_var`]).

mod guard;
pub mod keys;

pub use guard::{Caller, Guard, middleware};

use anyhow::{Context as _, Result, bail};
use jsonwebtoken::{Algorithm, Validation};
use nfi_auth::{AuthConfig, Whoami};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// CEL variables, by name.
pub type Vars = serde_json::Map<String, serde_json::Value>;

/// Used when no username expression is configured.
pub const DEFAULT_USERNAME_EXPR: &str =
    "has(claims.preferred_username) ? claims.preferred_username : claims.sub";

/// Used when no whoami expression is configured.
pub const DEFAULT_WHOAMI_EXPR: &str = "claims";

/// Clock skew allowed when checking `exp`/`nbf`.
const LEEWAY_SECS: u64 = 60;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("missing bearer token")]
    Missing,
    #[error("invalid token: {0}")]
    Invalid(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("identity provider unavailable: {0}")]
    Unavailable(String),
    #[error("{0}")]
    Internal(String),
}

impl AuthError {
    pub fn status(&self) -> http::StatusCode {
        use http::StatusCode as S;
        match self {
            AuthError::BadRequest(_) => S::BAD_REQUEST,
            AuthError::Missing | AuthError::Invalid(_) => S::UNAUTHORIZED,
            AuthError::Forbidden(_) => S::FORBIDDEN,
            AuthError::Unavailable(_) => S::SERVICE_UNAVAILABLE,
            AuthError::Internal(_) => S::INTERNAL_SERVER_ERROR,
        }
    }
}

/// The bearer token of a request, if any.
pub fn bearer(headers: &http::HeaderMap) -> Option<&str> {
    let v = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// Constant-time comparison of a presented token with the expected one.
pub fn token_matches(presented: &str, expected: &str) -> bool {
    let (a, b) = (presented.as_bytes(), expected.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Read a secret or expression from a file, without surrounding whitespace.
pub fn read_trimmed(path: &Path) -> Result<String> {
    let s = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let s = s.trim().to_string();
    if s.is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(s)
}

/// `{"method": ..., "path": ...}`, the `request` variable of HTTP policies,
/// for a request to `raw_path`: `path` is percent-decoded (see
/// [`decode_path`]), as route parameters are.
pub fn request_var(method: &http::Method, raw_path: &str) -> Result<serde_json::Value, AuthError> {
    Ok(serde_json::json!({"method": method.as_str(), "path": decode_path(raw_path)?}))
}

/// Percent-decode a request path. Rejected (400): invalid percent-encoding
/// or UTF-8, and an encoded `/`, which routing treats as part of a segment
/// but would look like a separator to a policy (no route needs one).
pub fn decode_path(raw: &str) -> Result<String, AuthError> {
    let bad = |what: &str| AuthError::BadRequest(format!("{what} in the path"));
    let hex = |c: Option<u8>| c.and_then(|c| char::from(c).to_digit(16));
    let mut out = Vec::with_capacity(raw.len());
    let mut bytes = raw.bytes();
    while let Some(c) = bytes.next() {
        if c != b'%' {
            out.push(c);
            continue;
        }
        match (hex(bytes.next()), hex(bytes.next())) {
            (Some(h), Some(l)) if h * 16 + l == u32::from(b'/') => return Err(bad("an encoded /")),
            (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
            _ => return Err(bad("invalid percent-encoding")),
        }
    }
    String::from_utf8(out).map_err(|_| bad("invalid UTF-8"))
}

/// A compiled CEL expression.
pub struct Expr {
    source: String,
    program: cel::Program,
}

impl std::fmt::Debug for Expr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Expr").field(&self.source).finish()
    }
}

impl Expr {
    pub fn new(source: &str) -> Result<Self> {
        let program = cel::Program::compile(source)
            .map_err(|e| anyhow::anyhow!("compiling CEL expression {source:?}: {e}"))?;
        Ok(Expr {
            source: source.to_string(),
            program,
        })
    }

    /// Compile the expression in a file (surrounding whitespace is ignored).
    pub fn from_file(path: &Path) -> Result<Self> {
        Self::new(&read_trimmed(path)?).with_context(|| format!("in {}", path.display()))
    }

    /// The expression in `path` if given, else `default`.
    pub fn from_file_or(path: Option<&Path>, default: &str) -> Result<Self> {
        match path {
            Some(p) => Self::from_file(p),
            None => Self::new(default),
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn eval(&self, vars: &Vars) -> Result<cel::Value> {
        let mut ctx = cel::Context::default();
        for (k, v) in vars {
            let v = cel::to_value(v).map_err(|e| anyhow::anyhow!("converting variable {k}: {e}"))?;
            ctx.add_variable_from_value(k.clone(), v);
        }
        self.program
            .execute(&ctx)
            .map_err(|e| anyhow::anyhow!("evaluating {:?}: {e}", self.source))
    }

    pub fn eval_bool(&self, vars: &Vars) -> Result<bool> {
        match self.eval(vars)? {
            cel::Value::Bool(b) => Ok(b),
            v => bail!("{:?} evaluated to {v:?}, not a bool", self.source),
        }
    }

    /// A string result; `null` is `None`.
    pub fn eval_string(&self, vars: &Vars) -> Result<Option<String>> {
        match self.eval(vars)? {
            cel::Value::String(s) => Ok(Some(s.to_string())),
            cel::Value::Null => Ok(None),
            v => bail!("{:?} evaluated to {v:?}, not a string", self.source),
        }
    }

    /// Evaluate a policy: `Forbidden` unless it is `true`. Evaluation
    /// errors are logged and also deny.
    pub fn authorize(&self, vars: &Vars) -> Result<(), AuthError> {
        match self.eval_bool(vars) {
            Ok(true) => Ok(()),
            Ok(false) => Err(AuthError::Forbidden("denied by policy".into())),
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "authorisation policy failed");
                Err(AuthError::Forbidden("authorisation policy failed".into()))
            }
        }
    }
}

/// An authenticated caller.
#[derive(Clone, Debug)]
pub struct Principal {
    pub username: String,
    pub session: Option<String>,
    pub claims: serde_json::Value,
}

impl Principal {
    /// `claims` and `user`.
    pub fn vars(&self) -> Vars {
        let mut v = Vars::new();
        v.insert("claims".into(), self.claims.clone());
        v.insert(
            "user".into(),
            serde_json::json!({"username": self.username, "session": self.session}),
        );
        v
    }
}

/// How a service validates tokens and identifies callers.
pub struct OidcSettings {
    pub issuer: String,
    pub client_id: String,
    /// Expected `aud` [default: `client_id`].
    pub audience: Option<String>,
    /// Scopes clients should request.
    pub scopes: Vec<String>,
    /// Username of the principal [default: [`DEFAULT_USERNAME_EXPR`]].
    pub username: Option<Expr>,
    /// Optional credential or session id of the principal, for logs.
    pub session: Option<Expr>,
    /// The `additional_info` object of whoami responses, over `claims` and
    /// `user` [default: [`DEFAULT_WHOAMI_EXPR`]].
    pub whoami: Option<Expr>,
}

impl OidcSettings {
    /// Settings for `issuer` and `client_id`, with every other field at its
    /// default.
    pub fn new(issuer: &str, client_id: &str) -> Self {
        OidcSettings {
            issuer: issuer.into(),
            client_id: client_id.into(),
            audience: None,
            scopes: vec![],
            username: None,
            session: None,
            whoami: None,
        }
    }

    /// Settings from service flags: the expressions are read from files.
    pub fn from_files(
        issuer: &str,
        client_id: &str,
        audience: Option<String>,
        scopes: Vec<String>,
        username_file: Option<&Path>,
        session_file: Option<&Path>,
        whoami_file: Option<&Path>,
    ) -> Result<Self> {
        let expr = |f: Option<&Path>| f.map(Expr::from_file).transpose();
        Ok(OidcSettings {
            audience,
            scopes,
            username: expr(username_file)?,
            session: expr(session_file)?,
            whoami: expr(whoami_file)?,
            ..Self::new(issuer, client_id)
        })
    }
}

/// Validates ID tokens and extracts principals. Discovery and JWKS are
/// fetched on first use (or by [`Authenticator::start`]) and cached as
/// described in [`keys`].
pub struct Authenticator {
    issuer: String,
    client_id: String,
    audience: String,
    scopes: Vec<String>,
    username: Expr,
    session: Option<Expr>,
    whoami: Expr,
    keys: Arc<keys::KeyCache>,
}

/// The default scopes advertised to clients.
pub fn default_scopes() -> Vec<String> {
    ["openid", "profile", "offline_access"].map(String::from).to_vec()
}

impl Authenticator {
    pub fn new(s: OidcSettings) -> Result<Self> {
        let issuer = s.issuer.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("nfi-auth/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Authenticator {
            keys: Arc::new(keys::KeyCache::new(issuer.clone(), http)),
            issuer,
            audience: s.audience.unwrap_or_else(|| s.client_id.clone()),
            client_id: s.client_id,
            scopes: if s.scopes.is_empty() {
                default_scopes()
            } else {
                s.scopes
            },
            username: match s.username {
                Some(e) => e,
                None => Expr::new(DEFAULT_USERNAME_EXPR)?,
            },
            session: s.session,
            whoami: match s.whoami {
                Some(e) => e,
                None => Expr::new(DEFAULT_WHOAMI_EXPR)?,
            },
        })
    }

    /// [`Authenticator::new`] and fetch the provider's keys, at service
    /// startup. A discovery document for another issuer is an error; an
    /// unreachable provider only a warning (keys are fetched on demand).
    pub async fn start(s: OidcSettings) -> Result<Arc<Self>> {
        let a = Self::new(s)?;
        match a.prefetch().await {
            Ok(()) => {}
            Err(AuthError::Unavailable(e)) => {
                tracing::warn!(error = %e, "could not fetch the OpenID provider's keys yet");
            }
            Err(e) => bail!("{e}"),
        }
        Ok(Arc::new(a))
    }

    /// Change how often keys are refetched for an unknown key id (default
    /// [`keys::DEFAULT_MIN_REFETCH`]) and when a key set is refreshed in the
    /// background (default [`keys::DEFAULT_MAX_AGE`]); for tests. Call it
    /// before the authenticator is used.
    pub fn with_key_refresh(mut self, min_refetch: Duration, max_age: Duration) -> Self {
        let k = Arc::get_mut(&mut self.keys).expect("the authenticator has not been used yet");
        k.min_refetch = min_refetch;
        k.max_age = max_age;
        self
    }

    /// The settings advertised at [`nfi_auth::CONFIG_PATH`] (`anonymous` is
    /// the [`Guard`]'s business and false here).
    pub fn config(&self) -> AuthConfig {
        AuthConfig {
            issuer: self.issuer.clone(),
            client_id: self.client_id.clone(),
            scopes: self.scopes.clone(),
            anonymous: false,
        }
    }

    /// Fetch discovery and keys now.
    pub async fn prefetch(&self) -> Result<(), AuthError> {
        self.keys.prefetch().await
    }

    /// The whoami response for `p`: `additional_info` is the whoami
    /// expression's result, which must be a map.
    pub fn whoami(&self, p: &Principal) -> Result<Whoami> {
        let v = self.whoami.eval(&p.vars())?;
        let json = v
            .json()
            .map_err(|e| anyhow::anyhow!("{:?}: converting the result to JSON: {e}", self.whoami.source()))?;
        let serde_json::Value::Object(additional_info) = json else {
            bail!("{:?} evaluated to {json}, not a map", self.whoami.source());
        };
        Ok(Whoami {
            username: p.username.clone(),
            session: p.session.clone(),
            additional_info,
        })
    }

    /// Validate an ID token and extract its principal.
    pub async fn authenticate(&self, token: &str) -> Result<Principal, AuthError> {
        let header = jsonwebtoken::decode_header(token).map_err(|e| AuthError::Invalid(e.to_string()))?;
        if matches!(header.alg, Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512) {
            return Err(AuthError::Invalid("symmetric signatures are not accepted".into()));
        }
        let key = self.keys.key(header.kid.as_deref(), header.alg).await?;
        let mut v = Validation::new(header.alg);
        v.set_issuer(&[&self.issuer, &format!("{}/", self.issuer)]);
        v.set_audience(&[&self.audience]);
        v.set_required_spec_claims(&["exp", "iss", "aud"]);
        v.leeway = LEEWAY_SECS;
        v.validate_nbf = true;
        let data = jsonwebtoken::decode::<serde_json::Value>(token, &key, &v)
            .map_err(|e| AuthError::Invalid(e.to_string()))?;
        let claims = data.claims;
        let mut vars = Vars::new();
        vars.insert("claims".into(), claims.clone());
        let username = match self.username.eval_string(&vars) {
            Ok(Some(u)) if !u.is_empty() => u,
            Ok(_) => return Err(AuthError::Forbidden("token has no username".into())),
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "username expression failed");
                return Err(AuthError::Forbidden("could not determine the username".into()));
            }
        };
        let session = match &self.session {
            None => None,
            Some(e) => e.eval_string(&vars).unwrap_or_else(|e| {
                tracing::warn!(error = %format!("{e:#}"), "session expression failed");
                None
            }),
        };
        Ok(Principal {
            username,
            session,
            claims,
        })
    }
}

pub(crate) async fn get_json<T: serde::de::DeserializeOwned>(http: &reqwest::Client, url: &str) -> Result<T> {
    let r = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    if !r.status().is_success() {
        bail!("fetching {url}: HTTP {}", r.status());
    }
    r.json().await.with_context(|| format!("decoding {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(v: serde_json::Value) -> Vars {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn expressions() {
        let v = vars(serde_json::json!({
            "claims": {"preferred_username": "alice", "groups": ["dev", "ops"], "sub": "123"},
            "user": {"username": "alice", "session": null},
        }));
        let e = Expr::new(DEFAULT_USERNAME_EXPR).unwrap();
        assert_eq!(e.eval_string(&v).unwrap().as_deref(), Some("alice"));
        let e = Expr::new("'ops' in claims.groups && user.username.startsWith('a')").unwrap();
        assert!(e.eval_bool(&v).unwrap());
        assert!(e.authorize(&v).is_ok());
        let e = Expr::new("'admin' in claims.groups").unwrap();
        assert!(matches!(e.authorize(&v), Err(AuthError::Forbidden(_))));
        let e = Expr::new("user.session").unwrap();
        assert_eq!(e.eval_string(&v).unwrap(), None);
        let e = Expr::new("claims.sub").unwrap();
        assert!(e.eval_bool(&v).is_err());
        let e = Expr::new("claims.missing == 1").unwrap();
        assert!(matches!(e.authorize(&v), Err(AuthError::Forbidden(_))));
        assert!(Expr::new("(((").is_err());
        let v = vars(serde_json::json!({"claims": {"sub": "123"}}));
        let e = Expr::new(DEFAULT_USERNAME_EXPR).unwrap();
        assert_eq!(e.eval_string(&v).unwrap().as_deref(), Some("123"));
    }

    #[test]
    fn paths() {
        assert_eq!(
            decode_path("/v1/packages/%69nternal-x%2b%20y").unwrap(),
            "/v1/packages/internal-x+ y"
        );
        assert_eq!(decode_path("/a/%C3%A9").unwrap(), "/a/\u{e9}");
        for p in ["/a%2Fb", "/a%2fb", "/a%zz", "/a%4", "/a%", "/a%ff", "/a%+1"] {
            assert!(matches!(decode_path(p), Err(AuthError::BadRequest(_))), "{p}");
        }
    }

    #[test]
    fn bearer_header() {
        let mut h = http::HeaderMap::new();
        assert_eq!(bearer(&h), None);
        h.insert(http::header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(bearer(&h), Some("abc"));
        h.insert(http::header::AUTHORIZATION, "bearer  xyz ".parse().unwrap());
        assert_eq!(bearer(&h), Some("xyz"));
        h.insert(http::header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert_eq!(bearer(&h), None);
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abc", "abd"));
        assert!(!token_matches("abc", "abcd"));
    }
}

//! Service side: ID token validation, principal extraction and CEL policies.
//!
//! An [`Authenticator`] validates an OIDC ID token against the issuer's JWKS
//! (signature, `iss`, `aud`, `exp`) and turns its claims into a
//! [`Principal`] with CEL expressions. Authorisation is a CEL expression
//! ([`Expr`]) that must evaluate to `true`; it sees the variables
//! [`Principal::vars`] provides plus any the service adds.
//!
//! CEL variables:
//! - `claims`: the token's claims, as a map.
//! - `user`: `{"username": string, "session": string or null}`.
//! - whatever else the service documents, e.g. `request`.

use crate::AuthConfig;
use anyhow::{Context as _, Result, bail};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::path::Path;
use std::time::{Duration, Instant};

/// CEL variables, by name.
pub type Vars = serde_json::Map<String, serde_json::Value>;

/// Used when no username expression is configured.
pub const DEFAULT_USERNAME_EXPR: &str =
    "has(claims.preferred_username) ? claims.preferred_username : claims.sub";

/// Refetch the JWKS for an unknown key id at most this often.
const JWKS_MIN_REFRESH: Duration = Duration::from_secs(30);
/// Clock skew allowed when checking `exp`/`nbf`.
const LEEWAY_SECS: u64 = 60;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
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

/// `{"method": ..., "path": ...}`, the `request` variable of HTTP policies.
pub fn request_var(method: &http::Method, path: &str) -> serde_json::Value {
    serde_json::json!({"method": method.as_str(), "path": path})
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

    pub fn whoami(&self) -> crate::Whoami {
        crate::Whoami {
            username: self.username.clone(),
            session: self.session.clone(),
        }
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
}

#[derive(Default)]
struct Keys {
    jwks_uri: Option<String>,
    set: Option<JwkSet>,
    fetched: Option<Instant>,
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
}

/// Validates ID tokens and extracts principals. Discovery and JWKS are
/// fetched on first use; an unknown key id triggers a (rate-limited) refetch.
pub struct Authenticator {
    issuer: String,
    client_id: String,
    audience: String,
    scopes: Vec<String>,
    username: Expr,
    session: Option<Expr>,
    http: reqwest::Client,
    keys: tokio::sync::RwLock<Keys>,
}

/// The default scopes advertised to clients.
pub fn default_scopes() -> Vec<String> {
    ["openid", "profile", "offline_access"].map(String::from).to_vec()
}

impl Authenticator {
    pub fn new(s: OidcSettings) -> Result<Self> {
        Ok(Authenticator {
            issuer: s.issuer.trim_end_matches('/').to_string(),
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
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("nfi-auth/", env!("CARGO_PKG_VERSION")))
                .build()?,
            keys: Default::default(),
        })
    }

    /// The settings advertised at [`crate::CONFIG_PATH`].
    pub fn config(&self) -> AuthConfig {
        AuthConfig {
            issuer: self.issuer.clone(),
            client_id: self.client_id.clone(),
            scopes: self.scopes.clone(),
        }
    }

    /// Fetch discovery and keys now, e.g. at startup.
    pub async fn prefetch(&self) -> Result<(), AuthError> {
        let mut k = self.keys.write().await;
        self.refresh(&mut k).await
    }

    async fn refresh(&self, k: &mut Keys) -> Result<(), AuthError> {
        let unavailable = |e: anyhow::Error| AuthError::Unavailable(format!("{e:#}"));
        k.fetched = Some(Instant::now());
        if k.jwks_uri.is_none() {
            let url = format!("{}/.well-known/openid-configuration", self.issuer);
            let d: Discovery = get_json(&self.http, &url).await.map_err(unavailable)?;
            if d.issuer.trim_end_matches('/') != self.issuer {
                return Err(AuthError::Internal(format!(
                    "issuer mismatch: discovery document at {url} is for {}",
                    d.issuer
                )));
            }
            k.jwks_uri = Some(d.jwks_uri);
        }
        let uri = k.jwks_uri.clone().expect("set above");
        k.set = Some(get_json(&self.http, &uri).await.map_err(unavailable)?);
        Ok(())
    }

    async fn key(&self, kid: Option<&str>) -> Result<DecodingKey, AuthError> {
        let find = |k: &Keys| -> Option<Result<DecodingKey, AuthError>> {
            let set = k.set.as_ref()?;
            let jwk = match kid {
                Some(kid) => set.find(kid)?,
                None if set.keys.len() == 1 => &set.keys[0],
                None => return None,
            };
            Some(DecodingKey::from_jwk(jwk).map_err(|e| AuthError::Internal(format!("unusable JWK: {e}"))))
        };
        if let Some(r) = find(&*self.keys.read().await) {
            return r;
        }
        let mut k = self.keys.write().await;
        if let Some(r) = find(&k) {
            return r;
        }
        if k.set.is_none() || k.fetched.is_none_or(|t| t.elapsed() >= JWKS_MIN_REFRESH) {
            self.refresh(&mut k).await?;
            if let Some(r) = find(&k) {
                return r;
            }
        }
        Err(AuthError::Invalid(format!(
            "unknown signing key {}",
            kid.unwrap_or("(no kid)")
        )))
    }

    /// Validate an ID token and extract its principal.
    pub async fn authenticate(&self, token: &str) -> Result<Principal, AuthError> {
        let header = jsonwebtoken::decode_header(token).map_err(|e| AuthError::Invalid(e.to_string()))?;
        if matches!(header.alg, Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512) {
            return Err(AuthError::Invalid("symmetric signatures are not accepted".into()));
        }
        let key = self.key(header.kid.as_deref()).await?;
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

async fn get_json<T: serde::de::DeserializeOwned>(http: &reqwest::Client, url: &str) -> Result<T> {
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

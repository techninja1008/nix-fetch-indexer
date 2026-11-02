//! The OpenID provider's signing keys (JWKS), cached.
//!
//! - A token whose key id is in the cached set never causes a fetch.
//! - An unknown key id refetches the set, at most once per
//!   [`KeyCache::min_refetch`] (failed attempts count), one fetch at a time;
//!   requests that waited for it use its result.
//! - A set older than [`KeyCache::max_age`] is refreshed in the background
//!   while the old one is still used, so revoked keys drop out eventually.
//! - Without any set (the provider has been unreachable since startup),
//!   requests fail with [`AuthError::Unavailable`].
//!
//! Fetches never happen under the snapshot lock; the refresh mutex
//! serialises them.

use crate::{AuthError, get_json};
use jsonwebtoken::jwk::{Jwk, JwkSet, KeyAlgorithm, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey};
use serde::Deserialize;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Refetch the JWKS for an unknown key id at most this often.
pub const DEFAULT_MIN_REFETCH: Duration = Duration::from_secs(30);
/// Refresh a key set in the background once it is this old.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(3600);

#[derive(Clone, Default)]
struct Snapshot {
    jwks_uri: Option<String>,
    set: Option<Arc<JwkSet>>,
    /// When `set` was fetched.
    fetched: Option<Instant>,
    /// The last fetch attempt, successful or not.
    attempted: Option<Instant>,
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
}

pub(crate) struct KeyCache {
    issuer: String,
    http: reqwest::Client,
    snapshot: RwLock<Arc<Snapshot>>,
    refresh: Arc<tokio::sync::Mutex<()>>,
    pub(crate) min_refetch: Duration,
    pub(crate) max_age: Duration,
}

impl KeyCache {
    pub(crate) fn new(issuer: String, http: reqwest::Client) -> Self {
        KeyCache {
            issuer,
            http,
            snapshot: Default::default(),
            refresh: Default::default(),
            min_refetch: DEFAULT_MIN_REFETCH,
            max_age: DEFAULT_MAX_AGE,
        }
    }

    fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.read().expect("not poisoned").clone()
    }

    fn due(&self, s: &Snapshot) -> bool {
        s.attempted.is_none_or(|t| t.elapsed() >= self.min_refetch)
    }

    /// Discovery (once) and the key set.
    async fn fetch(&self, jwks_uri: Option<String>) -> Result<(String, JwkSet), AuthError> {
        let unavailable = |e: anyhow::Error| AuthError::Unavailable(format!("{e:#}"));
        let uri = match jwks_uri {
            Some(u) => u,
            None => {
                let url = format!("{}/.well-known/openid-configuration", self.issuer);
                let d: Discovery = get_json(&self.http, &url).await.map_err(unavailable)?;
                if d.issuer.trim_end_matches('/') != self.issuer {
                    return Err(AuthError::Internal(format!(
                        "issuer mismatch: discovery document at {url} is for {}",
                        d.issuer
                    )));
                }
                d.jwks_uri
            }
        };
        let set = get_json(&self.http, &uri).await.map_err(unavailable)?;
        Ok((uri, set))
    }

    /// Fetch now and record the outcome. The caller holds `refresh`.
    async fn refresh_locked(&self) -> Result<(), AuthError> {
        let r = self.fetch(self.snapshot().jwks_uri.clone()).await;
        let now = Instant::now();
        let mut w = self.snapshot.write().expect("not poisoned");
        let mut s = Snapshot::clone(&w);
        s.attempted = Some(now);
        let r = r.map(|(uri, set)| {
            s.jwks_uri = Some(uri);
            s.set = Some(Arc::new(set));
            s.fetched = Some(now);
        });
        *w = Arc::new(s);
        r
    }

    /// Fetch discovery and keys now, e.g. at startup.
    pub(crate) async fn prefetch(&self) -> Result<(), AuthError> {
        let _g = self.refresh.lock().await;
        self.refresh_locked().await
    }

    /// The key for a token with this `kid` and `alg`.
    pub(crate) async fn key(
        self: &Arc<Self>,
        kid: Option<&str>,
        alg: Algorithm,
    ) -> Result<DecodingKey, AuthError> {
        let snap = self.snapshot();
        if snap.fetched.is_some_and(|t| t.elapsed() >= self.max_age)
            && self.due(&snap)
            && let Ok(g) = self.refresh.clone().try_lock_owned()
        {
            let me = self.clone();
            tokio::spawn(async move {
                if let Err(e) = me.refresh_locked().await {
                    tracing::warn!(error = %e, "could not refresh the OpenID provider's keys");
                }
                drop(g);
            });
        }
        if let Some(r) = find(&snap, kid, alg) {
            return r;
        }
        let _g = self.refresh.lock().await;
        let mut snap = self.snapshot();
        if find(&snap, kid, alg).is_none() && self.due(&snap) {
            let r = self.refresh_locked().await;
            snap = self.snapshot();
            match r {
                Err(e) if snap.set.is_none() => return Err(e),
                Err(e) => tracing::warn!(error = %e, "could not refresh the OpenID provider's keys"),
                Ok(()) => {}
            }
        }
        if let Some(r) = find(&snap, kid, alg) {
            return r;
        }
        if snap.set.is_none() {
            return Err(AuthError::Unavailable(
                "the OpenID provider's signing keys have not been fetched yet".into(),
            ));
        }
        Err(AuthError::Invalid(format!(
            "unknown signing key {}",
            kid.unwrap_or("(no kid)")
        )))
    }
}

/// The signing key `kid` names (without a kid, the only signing key), if any.
fn find(s: &Snapshot, kid: Option<&str>, alg: Algorithm) -> Option<Result<DecodingKey, AuthError>> {
    let set = s.set.as_ref()?;
    let signing = |j: &&Jwk| !matches!(j.common.public_key_use, Some(PublicKeyUse::Encryption));
    let mut keys = set.keys.iter().filter(signing);
    let jwk = match kid {
        Some(kid) => keys.find(|j| j.common.key_id.as_deref() == Some(kid))?,
        None => match (keys.next(), keys.next()) {
            (Some(j), None) => j,
            _ => return None,
        },
    };
    if let Some(a) = jwk.common.key_algorithm
        && a != KeyAlgorithm::from(alg)
    {
        return Some(Err(AuthError::Invalid(format!(
            "the token is signed with {alg:?} but its key is for {a}"
        ))));
    }
    Some(DecodingKey::from_jwk(jwk).map_err(|e| AuthError::Internal(format!("unusable JWK: {e}"))))
}

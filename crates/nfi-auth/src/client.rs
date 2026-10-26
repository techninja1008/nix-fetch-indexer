//! CLI side: device code login, token refresh and the token cache.
//!
//! Tokens are cached per (issuer, client id) in
//! `$XDG_CACHE_HOME/nfi/tokens/` (mode 0600), so every CLI talking to
//! services that share an OpenID provider and client shares one login. The
//! cache also remembers which [`AuthConfig`] each service URL advertised, so
//! a cached token is sent without asking the service first.

use crate::AuthConfig;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Tokens expiring within this many seconds are refreshed before use.
const EXPIRY_MARGIN_SECS: i64 = 60;

/// A cached login.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredToken {
    pub issuer: String,
    pub client_id: String,
    pub id_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

impl StoredToken {
    /// Whether the ID token is still usable (unknown expiry counts as usable).
    pub fn is_fresh(&self) -> bool {
        crate::jwt_expiry(&self.id_token).is_none_or(|exp| exp > now() + EXPIRY_MARGIN_SECS)
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn digest(parts: &[&str]) -> String {
    let mut h = sha2::Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    hex::encode(&h.finalize()[..16])
}

/// Normalise a service URL for use as a cache key.
fn service_key(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

/// The on-disk token cache.
#[derive(Clone, Debug)]
pub struct TokenStore {
    dir: PathBuf,
}

impl TokenStore {
    /// `$NFI_TOKEN_CACHE`, else `$XDG_CACHE_HOME/nfi/tokens`, else
    /// `~/.cache/nfi/tokens`.
    pub fn default_location() -> Result<Self> {
        if let Some(d) = std::env::var_os("NFI_TOKEN_CACHE") {
            return Ok(Self::at(d));
        }
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .context("neither XDG_CACHE_HOME nor HOME is set")?;
        Ok(Self::at(base.join("nfi").join("tokens")))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        TokenStore { dir: dir.into() }
    }

    fn token_path(&self, issuer: &str, client_id: &str) -> PathBuf {
        self.dir.join(format!("{}.json", digest(&[issuer, client_id])))
    }

    fn service_path(&self, url: &str) -> PathBuf {
        self.dir
            .join("services")
            .join(format!("{}.json", digest(&[&service_key(url)])))
    }

    fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
        match std::fs::read(path) {
            Ok(b) => Ok(Some(
                serde_json::from_slice(&b).with_context(|| format!("parsing {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write atomically, readable by the owner only.
    fn write<T: Serialize>(path: &Path, v: &T) -> Result<()> {
        let dir = path.parent().expect("has a parent");
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = dir.join(format!(".{}.tmp", std::process::id()));
        {
            use std::io::Write;
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
            let mut f = o
                .open(&tmp)
                .with_context(|| format!("writing {}", tmp.display()))?;
            f.write_all(&serde_json::to_vec_pretty(v)?)?;
        }
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }

    fn remove(path: &Path) -> Result<bool> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }

    pub fn token(&self, issuer: &str, client_id: &str) -> Result<Option<StoredToken>> {
        Self::read(&self.token_path(issuer, client_id))
    }

    pub fn save_token(&self, t: &StoredToken) -> Result<()> {
        Self::write(&self.token_path(&t.issuer, &t.client_id), t)
    }

    pub fn remove_token(&self, issuer: &str, client_id: &str) -> Result<bool> {
        Self::remove(&self.token_path(issuer, client_id))
    }

    /// The auth settings `url` advertised when last logged in to.
    pub fn service(&self, url: &str) -> Result<Option<AuthConfig>> {
        Self::read(&self.service_path(url))
    }

    pub fn save_service(&self, url: &str, c: &AuthConfig) -> Result<()> {
        Self::write(&self.service_path(url), c)
    }

    pub fn remove_service(&self, url: &str) -> Result<bool> {
        Self::remove(&self.service_path(url))
    }
}

/// What the user must do to approve a device login.
#[derive(Clone, Debug)]
pub struct DevicePrompt {
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
}

impl std::fmt::Display for DevicePrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.verification_uri_complete {
            Some(u) => write!(
                f,
                "To log in, open {u}\n(or visit {} and enter the code {})",
                self.verification_uri, self.user_code
            ),
            None => write!(
                f,
                "To log in, visit {} and enter the code {}",
                self.verification_uri, self.user_code
            ),
        }
    }
}

#[derive(Deserialize)]
struct ProviderMetadata {
    token_endpoint: String,
    device_authorization_endpoint: Option<String>,
}

#[derive(Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    #[serde(alias = "verification_url")]
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: Option<u64>,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    error_description: Option<String>,
}

/// Talks to services and OpenID providers on behalf of a CLI.
#[derive(Clone)]
pub struct Session {
    http: reqwest::Client,
    store: TokenStore,
}

impl Session {
    pub fn new(store: TokenStore) -> Result<Self> {
        Ok(Session {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .user_agent(concat!("nfi-auth/", env!("CARGO_PKG_VERSION")))
                .build()?,
            store,
        })
    }

    pub fn store(&self) -> &TokenStore {
        &self.store
    }

    /// The auth settings a service advertises; `None` if it has no OIDC.
    pub async fn discover(&self, url: &str) -> Result<Option<AuthConfig>> {
        let u = format!("{}{}", service_key(url), crate::CONFIG_PATH);
        let r = self
            .http
            .get(&u)
            .send()
            .await
            .with_context(|| format!("connecting to {url}"))?;
        match r.status() {
            s if s.is_success() => Ok(Some(r.json().await.with_context(|| format!("decoding {u}"))?)),
            reqwest::StatusCode::NOT_FOUND => Ok(None),
            s => bail!("fetching {u}: HTTP {s}"),
        }
    }

    async fn metadata(&self, issuer: &str) -> Result<ProviderMetadata> {
        let u = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let r = self
            .http
            .get(&u)
            .send()
            .await
            .with_context(|| format!("fetching {u}"))?;
        if !r.status().is_success() {
            bail!("fetching {u}: HTTP {}", r.status());
        }
        r.json().await.with_context(|| format!("decoding {u}"))
    }

    /// A cached, fresh ID token for a service, refreshing it if needed.
    /// `None` if the service was never logged in to or the login expired.
    pub async fn cached_token(&self, url: &str) -> Result<Option<String>> {
        let Some(cfg) = self.store.service(url)? else {
            return Ok(None);
        };
        self.cached_token_for(&cfg).await
    }

    /// [`Session::cached_token`] for known auth settings.
    pub async fn cached_token_for(&self, cfg: &AuthConfig) -> Result<Option<String>> {
        let Some(t) = self.store.token(&cfg.issuer, &cfg.client_id)? else {
            return Ok(None);
        };
        if t.is_fresh() {
            return Ok(Some(t.id_token));
        }
        let Some(rt) = &t.refresh_token else {
            return Ok(None);
        };
        match self.refresh(cfg, rt).await {
            Ok(Some(t)) => {
                self.store.save_token(&t)?;
                Ok(Some(t.id_token))
            }
            Ok(None) => Ok(None),
            // A rejected refresh token means logging in again; keep going
            // without a token rather than failing the command.
            Err(_) => Ok(None),
        }
    }

    async fn token_request(
        &self,
        endpoint: &str,
        form: &[(&str, &str)],
    ) -> Result<Result<TokenResponse, TokenError>> {
        let r = self
            .http
            .post(endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(form)
            .send()
            .await
            .with_context(|| format!("connecting to {endpoint}"))?;
        let status = r.status();
        let body = r.bytes().await?;
        if status.is_success() {
            return Ok(Ok(
                serde_json::from_slice(&body).context("decoding token response")?
            ));
        }
        match serde_json::from_slice::<TokenError>(&body) {
            Ok(e) => Ok(Err(e)),
            Err(_) => bail!(
                "token endpoint: HTTP {status}: {}",
                String::from_utf8_lossy(&body)
            ),
        }
    }

    /// Use a refresh token. `None` if the provider issued no new ID token.
    async fn refresh(&self, cfg: &AuthConfig, refresh_token: &str) -> Result<Option<StoredToken>> {
        let md = self.metadata(&cfg.issuer).await?;
        let scope = cfg.scopes.join(" ");
        let r = self
            .token_request(
                &md.token_endpoint,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                    ("client_id", &cfg.client_id),
                    ("scope", &scope),
                ],
            )
            .await?;
        let t = match r {
            Ok(t) => t,
            Err(e) => bail!("refreshing token: {}", e.error),
        };
        Ok(t.id_token.map(|id| StoredToken {
            issuer: cfg.issuer.clone(),
            client_id: cfg.client_id.clone(),
            id_token: id,
            refresh_token: t.refresh_token.or_else(|| Some(refresh_token.to_string())),
        }))
    }

    /// Run the device authorization grant, calling `prompt` once the user
    /// has something to do, and cache the result.
    pub async fn device_login(
        &self,
        cfg: &AuthConfig,
        prompt: impl FnOnce(&DevicePrompt),
    ) -> Result<StoredToken> {
        let md = self.metadata(&cfg.issuer).await?;
        let Some(dev_ep) = &md.device_authorization_endpoint else {
            bail!("{} does not support the device authorization grant", cfg.issuer);
        };
        let scope = cfg.scopes.join(" ");
        let r = self
            .http
            .post(dev_ep)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&[("client_id", cfg.client_id.as_str()), ("scope", &scope)])
            .send()
            .await
            .with_context(|| format!("connecting to {dev_ep}"))?;
        if !r.status().is_success() {
            let s = r.status();
            bail!(
                "device authorization: HTTP {s}: {}",
                r.text().await.unwrap_or_default()
            );
        }
        let d: DeviceAuthorization = r.json().await.context("decoding device authorization")?;
        prompt(&DevicePrompt {
            verification_uri: d.verification_uri.clone(),
            verification_uri_complete: d.verification_uri_complete.clone(),
            user_code: d.user_code.clone(),
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(d.expires_in.unwrap_or(600));
        let mut interval = Duration::from_secs(d.interval.unwrap_or(5));
        loop {
            tokio::time::sleep(interval).await;
            if std::time::Instant::now() > deadline {
                bail!("the login code expired");
            }
            let r = self
                .token_request(
                    &md.token_endpoint,
                    &[
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                        ("device_code", &d.device_code),
                        ("client_id", &cfg.client_id),
                    ],
                )
                .await?;
            match r {
                Ok(t) => {
                    let Some(id) = t.id_token else {
                        bail!("{} issued no ID token (is the openid scope allowed?)", cfg.issuer);
                    };
                    let t = StoredToken {
                        issuer: cfg.issuer.clone(),
                        client_id: cfg.client_id.clone(),
                        id_token: id,
                        refresh_token: t.refresh_token,
                    };
                    self.store.save_token(&t)?;
                    return Ok(t);
                }
                Err(e) if e.error == "authorization_pending" => {}
                Err(e) if e.error == "slow_down" => interval += Duration::from_secs(5),
                Err(e) => bail!(
                    "login failed: {}{}",
                    e.error,
                    e.error_description.map(|d| format!(": {d}")).unwrap_or_default()
                ),
            }
        }
    }

    /// Log in to a service: discover its settings, reuse a cached login for
    /// the same provider and client if there is one (unless `force`), and
    /// otherwise run the device grant. Remembers the service's settings.
    pub async fn login(
        &self,
        url: &str,
        force: bool,
        prompt: impl FnOnce(&DevicePrompt),
    ) -> Result<(AuthConfig, String)> {
        let Some(cfg) = self.discover(url).await? else {
            bail!("{url} does not use OIDC login");
        };
        self.store.save_service(url, &cfg)?;
        if !force && let Some(t) = self.cached_token_for(&cfg).await? {
            return Ok((cfg, t));
        }
        let t = self.device_login(&cfg, prompt).await?;
        Ok((cfg, t.id_token))
    }

    /// The token to send to a service: `explicit` (e.g. from `--token-file`
    /// or the environment) if given, else a cached login.
    pub async fn bearer(&self, url: &str, explicit: Option<String>) -> Result<Option<String>> {
        match explicit {
            Some(t) => Ok(Some(t)),
            None => self.cached_token(url).await,
        }
    }

    /// After a 401 from a service: log in interactively (prompting on
    /// stderr) if this is a terminal and the service uses OIDC. `rejected`
    /// says whether a token was sent and refused, in which case a cached
    /// login for the same provider is not reused. `None` if no login is
    /// possible.
    pub async fn relogin(&self, url: &str, rejected: bool) -> Result<Option<String>> {
        if !interactive() || self.discover(url).await?.is_none() {
            return Ok(None);
        }
        let (_, t) = self.login(url, rejected, |p| eprintln!("{p}")).await?;
        Ok(Some(t))
    }

    /// Forget a service's login. Returns whether there was one.
    pub async fn logout(&self, url: &str) -> Result<bool> {
        let cfg = match self.store.service(url)? {
            Some(c) => Some(c),
            None => self.discover(url).await.ok().flatten(),
        };
        let mut removed = self.store.remove_service(url)?;
        if let Some(c) = cfg {
            removed |= self.store.remove_token(&c.issuer, &c.client_id)?;
        }
        Ok(removed)
    }
}

/// Read a pre-obtained token (e.g. a CI-issued ID token) from a file.
pub fn read_token_file(path: &Path) -> Result<String> {
    let s = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let s = s.trim();
    if s.is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(s.to_string())
}

/// Whether a device login can be prompted for (stdin and stderr are terminals).
pub fn interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let s = TokenStore::at(d.path().join("t"));
        assert!(s.token("i", "c").unwrap().is_none());
        let t = StoredToken {
            issuer: "i".into(),
            client_id: "c".into(),
            id_token: "x.e30.y".into(),
            refresh_token: None,
        };
        s.save_token(&t).unwrap();
        assert_eq!(s.token("i", "c").unwrap().unwrap().id_token, "x.e30.y");
        assert!(s.token("i", "other").unwrap().is_none());
        let c = AuthConfig {
            issuer: "i".into(),
            client_id: "c".into(),
            scopes: vec!["openid".into()],
        };
        s.save_service("http://svc/", &c).unwrap();
        assert_eq!(s.service("http://svc").unwrap(), Some(c));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = s.token_path("i", "c");
            assert_eq!(std::fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(s.remove_token("i", "c").unwrap());
        assert!(!s.remove_token("i", "c").unwrap());
    }
}

//! CLI side: device code login, token refresh and the token cache.
//!
//! Tokens are cached per (issuer, client id) in
//! `$XDG_CACHE_HOME/nfi/tokens/` (mode 0600), so every CLI talking to
//! services that share an OpenID provider and client shares one login. A
//! service's [`AuthConfig`] is fetched once per process, never cached.

use anyhow::{Context, Result, bail};
use nfi_auth::oidc::{DeviceAuthorization, ProviderMetadata, TokenError, TokenResponse};
use nfi_auth::{AuthConfig, Whoami};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::OnceCell;

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

/// Whether an ID token is still usable (unknown expiry counts as usable).
fn fresh(token: &str) -> bool {
    jwt_expiry(token).is_none_or(|exp| exp > now() + EXPIRY_MARGIN_SECS)
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

    pub fn token(&self, issuer: &str, client_id: &str) -> Result<Option<StoredToken>> {
        let path = self.token_path(issuer, client_id);
        match std::fs::read(&path) {
            Ok(b) => Ok(Some(
                serde_json::from_slice(&b).with_context(|| format!("parsing {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write atomically; the temporary file is created with mode 0600.
    pub fn save_token(&self, t: &StoredToken) -> Result<()> {
        use std::io::Write;
        let path = self.token_path(&t.issuer, &t.client_id);
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let mut f = tempfile::NamedTempFile::new_in(&self.dir)?;
        f.write_all(&serde_json::to_vec_pretty(t)?)?;
        f.persist(&path)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn remove_token(&self, issuer: &str, client_id: &str) -> Result<bool> {
        let path = self.token_path(issuer, client_id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }

    /// Block until this process holds the refresh lock for a login; it is
    /// released when the file is dropped.
    fn lock(&self, issuer: &str, client_id: &str) -> Result<std::fs::File> {
        let path = self.token_path(issuer, client_id).with_extension("lock");
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        f.lock().with_context(|| format!("locking {}", path.display()))?;
        Ok(f)
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

/// What the token cache holds for a service.
enum Login {
    Usable(String),
    Missing,
    /// A login that expired and could not be refreshed.
    Expired,
}

/// The credentials a CLI sends to one service: an explicit token, or a
/// login (cached, refreshed, or made on demand).
pub struct Credentials {
    tool: &'static str,
    url: String,
    explicit: Option<String>,
    /// How to pass a token explicitly, e.g. "set NFI_TOKEN".
    token_hint: &'static str,
    /// `None` when there is nowhere to cache tokens (no HOME).
    store: Option<TokenStore>,
    http: reqwest::Client,
    discovered: OnceCell<Option<AuthConfig>>,
    current: Mutex<Option<String>>,
    noted_expiry: AtomicBool,
}

impl Credentials {
    /// `tool` is the command to suggest (`{tool} login`).
    pub fn new(tool: &'static str, url: &str, explicit: Option<String>, token_hint: &'static str) -> Self {
        Credentials {
            tool,
            url: url.trim_end_matches('/').to_string(),
            explicit,
            token_hint,
            store: TokenStore::default_location().ok(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .user_agent(concat!("nfi-auth/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("building the HTTP client"),
            discovered: OnceCell::new(),
            current: Mutex::new(None),
            noted_expiry: AtomicBool::new(false),
        }
    }

    /// Use `store` instead of the default token cache.
    pub fn with_store(mut self, store: TokenStore) -> Self {
        self.store = Some(store);
        self
    }

    /// The service's auth settings (fetched once); `None` if it has no OIDC.
    pub async fn config(&self) -> Result<Option<&AuthConfig>> {
        let c = self
            .discovered
            .get_or_try_init(|| async {
                let u = format!("{}{}", self.url, nfi_auth::CONFIG_PATH);
                let r = self
                    .http
                    .get(&u)
                    .send()
                    .await
                    .with_context(|| format!("connecting to {}", self.url))?;
                match r.status() {
                    s if s.is_success() => Ok(Some(r.json().await.with_context(|| format!("decoding {u}"))?)),
                    reqwest::StatusCode::NOT_FOUND => Ok(None),
                    s => bail!("fetching {u}: HTTP {s}"),
                }
            })
            .await?;
        Ok(c.as_ref())
    }

    /// The token for a command; logs in (if `prompt` and stderr is a
    /// terminal) or fails with the hint when one is needed.
    pub async fn token(&self, prompt: bool) -> Result<Option<String>> {
        if let Some(t) = &self.explicit {
            return Ok(Some(t.clone()));
        }
        let Some(cfg) = self.config().await? else {
            return Ok(None);
        };
        let expired = match self.stored(cfg).await? {
            Login::Usable(t) => return Ok(Some(t)),
            Login::Missing => false,
            Login::Expired => true,
        };
        if cfg.anonymous {
            if expired && !self.noted_expiry.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "note: the login for {} expired; continuing without it (run `{} login`)",
                    self.url, self.tool
                );
            }
            return Ok(None);
        }
        if self.store.is_none() {
            bail!(
                "authentication required: set HOME or NFI_TOKEN_CACHE to log in, or {}",
                self.token_hint
            );
        }
        if prompt && interactive() {
            return self.device(cfg).await.map(Some);
        }
        bail!("{}", self.hint())
    }

    /// A usable login if there is one (refreshing it), never prompting or
    /// failing for lack of one (for `whoami`).
    pub async fn current(&self) -> Result<Option<String>> {
        if let Some(t) = &self.explicit {
            return Ok(Some(t.clone()));
        }
        let Some(cfg) = self.config().await? else {
            return Ok(None);
        };
        Ok(match self.stored(cfg).await? {
            Login::Usable(t) => Some(t),
            Login::Missing | Login::Expired => None,
        })
    }

    /// Log in: unless `force`, keep a cached login the service accepts;
    /// otherwise run the device flow. Returns who the service sees.
    pub async fn login(&self, force: bool) -> Result<Whoami> {
        let cfg = self
            .config()
            .await?
            .with_context(|| format!("{} does not use OIDC login", self.url))?;
        if !force
            && let Login::Usable(t) = self.stored(cfg).await?
            && let Some(w) = self.whoami(&t).await?
        {
            return Ok(w);
        }
        let t = self.device(cfg).await?;
        self.whoami(&t)
            .await?
            .with_context(|| format!("{} rejected the new login", self.url))
    }

    /// Forget the provider login the service uses (and so the login of
    /// every service sharing it). Returns whether there was one.
    pub async fn logout(&self) -> Result<bool> {
        *self.current.lock().unwrap() = None;
        match (self.config().await?, &self.store) {
            (Some(cfg), Some(store)) => store.remove_token(&cfg.issuer, &cfg.client_id),
            _ => Ok(false),
        }
    }

    /// After a 401: forget the token in use and log in again, if that is
    /// possible (no explicit token, OIDC, stderr a terminal).
    pub async fn relogin(&self) -> Result<Option<String>> {
        *self.current.lock().unwrap() = None;
        if self.explicit.is_some() || !interactive() {
            return Ok(None);
        }
        match self.config().await? {
            Some(cfg) => self.device(cfg).await.map(Some),
            None => Ok(None),
        }
    }

    /// What to do about a 401.
    pub fn hint(&self) -> String {
        if self.explicit.is_some() {
            return "the token was rejected".into();
        }
        match self.discovered.get() {
            Some(Some(_)) => format!(
                "authentication required: run `{} login` or {}",
                self.tool, self.token_hint
            ),
            _ => format!("authentication required: {}", self.token_hint),
        }
    }

    fn remember(&self, t: String) -> String {
        *self.current.lock().unwrap() = Some(t.clone());
        t
    }

    /// The token held in memory or the cache, refreshing it if needed.
    async fn stored(&self, cfg: &AuthConfig) -> Result<Login> {
        if let Some(t) = self.current.lock().unwrap().clone()
            && fresh(&t)
        {
            return Ok(Login::Usable(t));
        }
        let Some(store) = &self.store else {
            return Ok(Login::Missing);
        };
        match store.token(&cfg.issuer, &cfg.client_id)? {
            None => return Ok(Login::Missing),
            Some(t) if fresh(&t.id_token) => return Ok(Login::Usable(self.remember(t.id_token))),
            Some(StoredToken {
                refresh_token: None, ..
            }) => return Ok(Login::Expired),
            Some(_) => {}
        }
        let (s, i, c) = (store.clone(), cfg.issuer.clone(), cfg.client_id.clone());
        let _lock = tokio::task::spawn_blocking(move || s.lock(&i, &c)).await??;
        // Another process may have refreshed it meanwhile.
        let Some(t) = store.token(&cfg.issuer, &cfg.client_id)? else {
            return Ok(Login::Missing);
        };
        if fresh(&t.id_token) {
            return Ok(Login::Usable(self.remember(t.id_token)));
        }
        let Some(rt) = &t.refresh_token else {
            return Ok(Login::Expired);
        };
        match self.refresh(cfg, rt).await {
            Ok(Some(t)) => {
                store.save_token(&t)?;
                Ok(Login::Usable(self.remember(t.id_token)))
            }
            Ok(None) => {
                store.remove_token(&cfg.issuer, &cfg.client_id)?;
                Ok(Login::Expired)
            }
            Err(e) if cfg.anonymous => {
                eprintln!("warning: not using the login for {}: {e:#}", self.url);
                Ok(Login::Missing)
            }
            Err(e) => Err(e.context(format!("refreshing the login for {}", self.url))),
        }
    }

    /// Run the device flow (prompting on stderr) and cache the login.
    async fn device(&self, cfg: &AuthConfig) -> Result<String> {
        let store = self
            .store
            .as_ref()
            .context("no token cache: set HOME, XDG_CACHE_HOME or NFI_TOKEN_CACHE")?;
        let t = self.device_login(cfg).await?;
        store.save_token(&t)?;
        Ok(self.remember(t.id_token))
    }

    /// Who the service sees with `token`; `None` on a 401.
    async fn whoami(&self, token: &str) -> Result<Option<Whoami>> {
        let u = format!("{}{}", self.url, nfi_auth::WHOAMI_PATH);
        let r = self
            .http
            .get(&u)
            .bearer_auth(token)
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.url))?;
        match r.status() {
            reqwest::StatusCode::UNAUTHORIZED => Ok(None),
            s if s.is_success() => Ok(Some(r.json().await.with_context(|| format!("decoding {u}"))?)),
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

    /// Use a refresh token. `None` if the provider rejects it or issues no
    /// new ID token.
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
        Ok(r.ok().and_then(|t| {
            Some(StoredToken {
                issuer: cfg.issuer.clone(),
                client_id: cfg.client_id.clone(),
                id_token: t.id_token?,
                refresh_token: t.refresh_token.or_else(|| Some(refresh_token.to_string())),
            })
        }))
    }

    /// The device authorization grant, prompting on stderr.
    async fn device_login(&self, cfg: &AuthConfig) -> Result<StoredToken> {
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
        eprintln!(
            "{}",
            DevicePrompt {
                verification_uri: d.verification_uri.clone(),
                verification_uri_complete: d.verification_uri_complete.clone(),
                user_code: d.user_code.clone(),
            }
        );
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
                    return Ok(StoredToken {
                        issuer: cfg.issuer.clone(),
                        client_id: cfg.client_id.clone(),
                        id_token: id,
                        refresh_token: t.refresh_token,
                    });
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
}

/// `alice` or `alice (session s1)`.
pub fn describe(w: &Whoami) -> String {
    match &w.session {
        Some(s) => format!("{} (session {s})", w.username),
        None => w.username.clone(),
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

/// A token given explicitly: from `file`, else the environment variable
/// `env` (trimmed; empty means none).
pub fn explicit(file: Option<&Path>, env: &str) -> Result<Option<String>> {
    if let Some(f) = file {
        return Ok(Some(read_token_file(f)?));
    }
    Ok(std::env::var(env)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty()))
}

/// Whether a device login can be prompted for (stderr is a terminal; the
/// device flow never reads stdin, so it may be a pipe).
pub fn interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

/// Read the unverified `exp` claim of a JWT (seconds since the epoch).
pub fn jwt_expiry(token: &str) -> Option<i64> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("exp")?.as_i64()
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = s.token_path("i", "c");
            assert_eq!(std::fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(s.remove_token("i", "c").unwrap());
        assert!(!s.remove_token("i", "c").unwrap());
    }

    #[test]
    fn expiry() {
        use base64::Engine;
        let enc = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        let t = format!("{}.{}.sig", enc("{}"), enc(r#"{"exp":1234,"sub":"x"}"#));
        assert_eq!(super::jwt_expiry(&t), Some(1234));
        assert_eq!(super::jwt_expiry("garbage"), None);
    }
}

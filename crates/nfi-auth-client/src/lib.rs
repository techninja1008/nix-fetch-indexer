//! CLI side: device code login, token refresh and the token cache.
//!
//! Tokens are cached per (issuer, client id) in
//! `$XDG_CACHE_HOME/nfi/tokens/` (mode 0600), next to a record of the
//! service URLs each was logged in to. A cached token is only sent to those
//! URLs: every CLI talking to one of them shares the login. A service's
//! [`AuthConfig`] is fetched once per process, never cached.

pub mod http;

use anyhow::{Context, Result, bail};
use nfi_auth::oidc::{DeviceAuthorization, ProviderMetadata, TokenError, TokenResponse};
use nfi_auth::{AuthConfig, Whoami};
use serde::de::DeserializeOwned;
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

/// The provider login a service URL was logged in with.
#[derive(Debug, Serialize, Deserialize)]
struct UrlLogin {
    url: String,
    issuer: String,
    client_id: String,
}

/// Whether a corrupt cache file was reported already.
static WARNED_CORRUPT: AtomicBool = AtomicBool::new(false);

/// Read a cache file; a missing or corrupt one (reported once) is `None`.
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(v) => Ok(Some(v)),
            Err(e) => {
                if !WARNED_CORRUPT.swap(true, Ordering::Relaxed) {
                    eprintln!("warning: ignoring {}: {e}", path.display());
                }
                Ok(None)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn remove(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Create `dir` (and its parents) accessible by the owner only. An
/// existing `dir` of ours (older versions made it 0755) is made so too.
fn create_dir(dir: &Path) -> Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let m = std::fs::metadata(dir).with_context(|| format!("reading {}", dir.display()))?;
        // SAFETY: geteuid has no preconditions and cannot fail.
        if m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o777 != 0o700 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("setting the mode of {}", dir.display()))?;
        }
    }
    Ok(())
}

/// The on-disk token cache.
#[derive(Clone, Debug)]
pub struct TokenStore {
    dir: PathBuf,
}

impl TokenStore {
    /// `$NFI_TOKEN_CACHE`, else `$XDG_CACHE_HOME/nfi/tokens`, else
    /// `~/.cache/nfi/tokens`.
    fn default_location() -> Result<Self> {
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

    fn urls_dir(&self) -> PathBuf {
        self.dir.join("urls")
    }

    fn url_path(&self, url: &str) -> PathBuf {
        self.urls_dir()
            .join(format!("{}.json", digest(&[url.trim_end_matches('/')])))
    }

    /// Create the store's directories (see [`create_dir`]).
    fn create_dirs(&self) -> Result<()> {
        create_dir(&self.dir)?;
        create_dir(&self.urls_dir())
    }

    /// Write atomically; the temporary file is created with mode 0600.
    fn write<T: Serialize>(&self, path: &Path, v: &T) -> Result<()> {
        use std::io::Write;
        let dir = path.parent().expect("has a parent");
        self.create_dirs()?;
        let mut f = tempfile::NamedTempFile::new_in(dir)?;
        f.write_all(&serde_json::to_vec_pretty(v)?)?;
        f.persist(path)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn token(&self, issuer: &str, client_id: &str) -> Result<Option<StoredToken>> {
        read_json(&self.token_path(issuer, client_id))
    }

    pub fn save_token(&self, t: &StoredToken) -> Result<()> {
        self.write(&self.token_path(&t.issuer, &t.client_id), t)
    }

    pub fn remove_token(&self, issuer: &str, client_id: &str) -> Result<bool> {
        remove(&self.token_path(issuer, client_id))
    }

    /// Record that `url` was logged in to with the provider login
    /// (`issuer`, `client_id`): its token may be sent there.
    pub fn record_url(&self, url: &str, issuer: &str, client_id: &str) -> Result<()> {
        let r = UrlLogin {
            url: url.trim_end_matches('/').into(),
            issuer: issuer.into(),
            client_id: client_id.into(),
        };
        self.write(&self.url_path(url), &r)
    }

    /// The provider login `url` was logged in with, if any.
    fn url_login(&self, url: &str) -> Result<Option<UrlLogin>> {
        let r: Option<UrlLogin> = read_json(&self.url_path(url))?;
        Ok(r.filter(|r| r.url == url.trim_end_matches('/')))
    }

    /// Whether `url` was logged in to with (`issuer`, `client_id`).
    fn is_recorded(&self, url: &str, issuer: &str, client_id: &str) -> Result<bool> {
        Ok(self
            .url_login(url)?
            .is_some_and(|r| (r.issuer.as_str(), r.client_id.as_str()) == (issuer, client_id)))
    }

    /// The URLs logged in to with (`issuer`, `client_id`).
    fn urls(&self, issuer: &str, client_id: &str) -> Result<Vec<String>> {
        let dir = self.urls_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        let mut urls = vec![];
        for e in entries {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "json")
                && let Some(r) = read_json::<UrlLogin>(&p)?
                && (r.issuer.as_str(), r.client_id.as_str()) == (issuer, client_id)
            {
                urls.push(r.url);
            }
        }
        urls.sort();
        Ok(urls)
    }

    /// Block until this process holds the refresh lock for a login; it is
    /// released when the file is dropped.
    fn lock(&self, issuer: &str, client_id: &str) -> Result<std::fs::File> {
        let path = self.token_path(issuer, client_id).with_extension("lock");
        self.create_dirs()?;
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
struct DevicePrompt {
    verification_uri: String,
    verification_uri_complete: Option<String>,
    user_code: String,
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

/// Who a service sees with the credentials sent to it.
#[derive(Debug)]
pub enum Identity {
    User(Whoami),
    NotAuthenticated,
    /// The service does not use OIDC login.
    NoLogin,
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
    /// For the service.
    http: reqwest::Client,
    /// For the provider: never follows redirects, which would replay a
    /// refresh token or device code to wherever they point.
    idp: reqwest::Client,
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
            http: client(reqwest::redirect::Policy::default()),
            idp: client(reqwest::redirect::Policy::none()),
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

    /// Who the service sees with [`Credentials::current`], without logging
    /// in.
    pub async fn whoami(&self) -> Result<Identity> {
        let t = self.current().await?;
        self.ask(t.as_deref()).await
    }

    /// Log in: unless `force`, keep a cached provider login the service
    /// accepts (even one made for another URL: the user chose to log in
    /// here; if using it fails, with a warning); otherwise run the device
    /// flow. Either way the URL is recorded as logged in. Returns who the
    /// service sees.
    pub async fn login(&self, force: bool) -> Result<Whoami> {
        let cfg = self
            .config()
            .await?
            .with_context(|| format!("{} does not use OIDC login", self.url))?;
        if !force && let Some(store) = &self.store {
            match self.reuse(store, cfg).await {
                Ok(Some((t, w))) => {
                    store.record_url(&self.url, &cfg.issuer, &cfg.client_id)?;
                    self.remember(t);
                    return Ok(w);
                }
                Ok(None) => {}
                Err(e) => eprintln!("warning: not using the cached login: {e:#}"),
            }
        }
        let t = self.device(cfg).await?;
        match self.ask(Some(&t)).await? {
            Identity::User(w) => Ok(w),
            _ => {
                // Keep the provider login, but stop sending it here.
                *self.current.lock().unwrap() = None;
                if let Some(store) = &self.store {
                    remove(&store.url_path(&self.url))?;
                }
                bail!("{} rejected the new login", self.url)
            }
        }
    }

    /// The cached provider login and who the service sees with it, if it
    /// accepts it.
    async fn reuse(&self, store: &TokenStore, cfg: &AuthConfig) -> Result<Option<(String, Whoami)>> {
        let Login::Usable(t) = self.provider(store, cfg).await? else {
            return Ok(None);
        };
        Ok(match self.ask(Some(&t)).await? {
            Identity::User(w) => Some((t, w)),
            _ => None,
        })
    }

    /// Log out of the service: forget the provider login it was logged in
    /// with, which also logs out of every other URL logged in with it.
    /// `None` if it was not logged in (it has no record: nothing is
    /// removed), else those other URLs. Works offline.
    pub async fn logout(&self) -> Result<Option<Vec<String>>> {
        *self.current.lock().unwrap() = None;
        let Some(store) = &self.store else {
            return Ok(None);
        };
        let Some(UrlLogin {
            issuer, client_id, ..
        }) = store.url_login(&self.url)?
        else {
            return Ok(None);
        };
        // Not while another process refreshes it, which would save it again.
        let _lock = self.lock(store, &issuer, &client_id).await?;
        let urls = store.urls(&issuer, &client_id)?;
        for u in &urls {
            remove(&store.url_path(u))?;
        }
        let removed = store.remove_token(&issuer, &client_id)?;
        let was = removed || urls.contains(&self.url);
        Ok(was.then(|| urls.into_iter().filter(|u| *u != self.url).collect()))
    }

    /// After a 401: forget the token in use and log in again, if that is
    /// possible (no explicit token, a token cache, OIDC, stderr a terminal).
    pub async fn relogin(&self) -> Result<Option<String>> {
        *self.current.lock().unwrap() = None;
        if self.explicit.is_some() || self.store.is_none() || !interactive() {
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

    /// The login for this URL held in memory or the cache, refreshing it if
    /// needed. A cached login is only used if this URL was logged in to with
    /// it, so a service merely claiming the same provider never gets it.
    async fn stored(&self, cfg: &AuthConfig) -> Result<Login> {
        if let Some(t) = self.current.lock().unwrap().clone()
            && fresh(&t)
        {
            return Ok(Login::Usable(t));
        }
        let Some(store) = &self.store else {
            return Ok(Login::Missing);
        };
        if !store.is_recorded(&self.url, &cfg.issuer, &cfg.client_id)? {
            return Ok(Login::Missing);
        }
        let l = self.provider(store, cfg).await?;
        if let Login::Usable(t) = &l {
            self.remember(t.clone());
        }
        Ok(l)
    }

    /// Take the lock for refreshing, replacing or removing a provider login.
    async fn lock(&self, store: &TokenStore, issuer: &str, client_id: &str) -> Result<std::fs::File> {
        let (s, i, c) = (store.clone(), issuer.to_string(), client_id.to_string());
        tokio::task::spawn_blocking(move || s.lock(&i, &c)).await?
    }

    /// The cached provider login (whatever URLs it is for), refreshing it if
    /// needed.
    async fn provider(&self, store: &TokenStore, cfg: &AuthConfig) -> Result<Login> {
        match store.token(&cfg.issuer, &cfg.client_id)? {
            None => return Ok(Login::Missing),
            Some(t) if fresh(&t.id_token) => return Ok(Login::Usable(t.id_token)),
            Some(StoredToken {
                refresh_token: None, ..
            }) => return Ok(Login::Expired),
            Some(_) => {}
        }
        let _lock = self.lock(store, &cfg.issuer, &cfg.client_id).await?;
        // Another process may have refreshed it meanwhile.
        let Some(t) = store.token(&cfg.issuer, &cfg.client_id)? else {
            return Ok(Login::Missing);
        };
        if fresh(&t.id_token) {
            return Ok(Login::Usable(t.id_token));
        }
        let Some(rt) = &t.refresh_token else {
            return Ok(Login::Expired);
        };
        match self.refresh(cfg, rt).await {
            Ok(Some(t)) => {
                store.save_token(&t)?;
                Ok(Login::Usable(t.id_token))
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

    /// Run the device flow (prompting on stderr), cache the login and
    /// record this URL as logged in with it.
    async fn device(&self, cfg: &AuthConfig) -> Result<String> {
        let store = self
            .store
            .as_ref()
            .context("no token cache: set HOME, XDG_CACHE_HOME or NFI_TOKEN_CACHE")?;
        let t = self.device_login(cfg).await?;
        let _lock = self.lock(store, &cfg.issuer, &cfg.client_id).await?;
        store.save_token(&t)?;
        store.record_url(&self.url, &cfg.issuer, &cfg.client_id)?;
        Ok(self.remember(t.id_token))
    }

    /// Who the service sees with `token`.
    async fn ask(&self, token: Option<&str>) -> Result<Identity> {
        let u = format!("{}{}", self.url, nfi_auth::WHOAMI_PATH);
        let mut rb = self.http.get(&u);
        if let Some(t) = token {
            rb = rb.bearer_auth(t);
        }
        let r = rb
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.url))?;
        Ok(match r.status() {
            reqwest::StatusCode::UNAUTHORIZED => Identity::NotAuthenticated,
            reqwest::StatusCode::NOT_FOUND => Identity::NoLogin,
            _ => Identity::User(
                http::check(r)
                    .await?
                    .json()
                    .await
                    .with_context(|| format!("decoding {u}"))?,
            ),
        })
    }

    async fn metadata(&self, issuer: &str) -> Result<ProviderMetadata> {
        let u = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let r = self
            .idp
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
            .idp
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

    /// Use a refresh token. `None` if the provider says it is no longer
    /// valid (`invalid_grant`); other errors (the provider unavailable, a
    /// misconfigured client...) leave it usable later.
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
            Err(e) if e.error == "invalid_grant" => return Ok(None),
            Err(e) => bail!("{}", oauth_error(&e)),
        };
        let Some(id_token) = t.id_token else {
            bail!("{} issued no ID token", cfg.issuer);
        };
        Ok(Some(StoredToken {
            issuer: cfg.issuer.clone(),
            client_id: cfg.client_id.clone(),
            id_token,
            refresh_token: t.refresh_token.or_else(|| Some(refresh_token.to_string())),
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
            .idp
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
                Err(e) => bail!("login failed: {}", oauth_error(&e)),
            }
        }
    }
}

fn client(redirect: reqwest::redirect::Policy) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("nfi-auth/", env!("CARGO_PKG_VERSION")))
        .redirect(redirect)
        .build()
        .expect("building the HTTP client")
}

/// `invalid_grant` or `invalid_grant: description`.
fn oauth_error(e: &TokenError) -> String {
    match &e.error_description {
        Some(d) => format!("{}: {d}", e.error),
        None => e.error.clone(),
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
fn read_token_file(path: &Path) -> Result<String> {
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
fn interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

/// Read the unverified `exp` claim of a JWT (seconds since the epoch).
fn jwt_expiry(token: &str) -> Option<i64> {
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

        // URL records, keyed by the URL without a trailing slash.
        s.record_url("http://a/", "i", "c").unwrap();
        s.record_url("http://b", "i", "c").unwrap();
        s.record_url("http://c", "i", "other").unwrap();
        assert!(s.is_recorded("http://a", "i", "c").unwrap());
        assert!(!s.is_recorded("http://c", "i", "c").unwrap());
        assert!(!s.is_recorded("http://d", "i", "c").unwrap());
        assert_eq!(s.urls("i", "c").unwrap(), ["http://a", "http://b"]);

        // A corrupt file counts as missing.
        std::fs::write(s.token_path("i", "c"), "{").unwrap();
        assert!(s.token("i", "c").unwrap().is_none());
        std::fs::write(s.url_path("http://a"), "").unwrap();
        assert!(!s.is_recorded("http://a", "i", "c").unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [s.dir.clone(), s.urls_dir()] {
                assert_eq!(
                    std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
        }
    }

    /// Directories made by older versions (0755) are made 0700.
    #[cfg(unix)]
    #[test]
    fn store_dirs_are_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let s = TokenStore::at(d.path().join("t"));
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        for dir in [s.dir.clone(), s.urls_dir()] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        s.record_url("http://a", "i", "c").unwrap();
        assert_eq!((mode(s.dir.clone()), mode(s.urls_dir())), (0o700, 0o700));
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

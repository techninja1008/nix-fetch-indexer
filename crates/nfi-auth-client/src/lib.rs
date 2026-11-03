//! CLI side: device code login, token refresh and the token cache.
//!
//! Tokens are cached per (issuer, client id) in
//! `$XDG_CACHE_HOME/nfi/tokens/` (mode 0600), next to a record of the
//! service URLs each was logged in to and the [`AuthConfig`] each advertised
//! then. A cached token is only sent to those URLs: every CLI talking to one
//! of them shares the login. The settings of other URLs are cached too
//! (`settings/`), which never makes them receive a token. A service's
//! settings are fetched on first use and then again only by `login` and
//! after it rejects a request (a 401). Tokens are never sent, and logins
//! never made, over plain HTTP to hosts other than loopback unless that is
//! explicitly allowed. The cache's directories must be owned by the user
//! and inaccessible to others (as they are created).
//!
//! A URL without a record is never sent a cached login by itself: when a
//! command or `login` needs one, it asks on the terminal whether to use the
//! provider login made for other URLs (if the URL advertises the same
//! provider and client), else runs the device flow.

pub mod http;

use anyhow::{Context, Result, bail};
use nfi_auth::oidc::{DeviceAuthorization, ProviderMetadata, TokenError, TokenResponse};
use nfi_auth::{AuthConfig, Whoami};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::OnceCell;

/// Tokens expiring within this many seconds are refreshed before use.
const EXPIRY_MARGIN_SECS: i64 = 60;

/// How to give the CLIs a token cache.
const NO_CACHE_HINT: &str = "set HOME, XDG_CACHE_HOME or NFI_TOKEN_CACHE";

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

/// The settings a service URL advertised when it was last logged in to
/// (so with which provider login).
#[derive(Debug, Serialize, Deserialize)]
struct UrlLogin {
    url: String,
    issuer: String,
    client_id: String,
    scopes: Vec<String>,
    anonymous: bool,
}

impl UrlLogin {
    fn config(self) -> AuthConfig {
        AuthConfig {
            issuer: self.issuer,
            client_id: self.client_id,
            scopes: self.scopes,
            anonymous: self.anonymous,
        }
    }
}

/// The settings a service URL advertised when last asked: `None` if it has
/// no OIDC.
#[derive(Debug, Serialize, Deserialize)]
struct UrlSettings {
    url: String,
    config: Option<AuthConfig>,
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

/// Create `dir` (and its parents) accessible by the owner only.
fn create_dir(dir: &Path) -> Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// Fail unless `dir` (if it exists) is a directory owned by the effective
/// user and inaccessible to anyone else; `Ok(false)` if it does not exist.
fn private_dir(dir: &Path) -> Result<bool> {
    let m = match std::fs::metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    if !m.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        if m.uid() != me {
            bail!(
                "refusing to use the token cache {}: it is owned by uid {}, not by you (uid {me})",
                dir.display(),
                m.uid()
            );
        }
        if m.mode() & 0o077 != 0 {
            bail!(
                "refusing to use the token cache {}: others have access to it (mode {:o}); \
                 make it private with `chmod 700`",
                dir.display(),
                m.mode() & 0o7777
            );
        }
    }
    Ok(true)
}

/// The on-disk token cache.
#[derive(Clone, Debug)]
pub struct TokenStore {
    dir: PathBuf,
    /// Set once all its directories were found private (see [`private_dir`]).
    checked: Arc<OnceLock<()>>,
}

impl TokenStore {
    /// `$NFI_TOKEN_CACHE`, else `$XDG_CACHE_HOME/nfi/tokens`, else
    /// `~/.cache/nfi/tokens`; `None` if neither is set.
    fn default_location() -> Option<Self> {
        if let Some(d) = std::env::var_os("NFI_TOKEN_CACHE") {
            return Some(Self::at(d));
        }
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
        Some(Self::at(base.join("nfi").join("tokens")))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        TokenStore {
            dir: dir.into(),
            checked: Arc::default(),
        }
    }

    /// Fail unless the store's directories are private (see
    /// [`private_dir`]): checked before each use until all exist.
    fn check(&self) -> Result<()> {
        if self.checked.get().is_some() {
            return Ok(());
        }
        let mut all = true;
        for d in [self.dir.clone(), self.urls_dir(), self.dir.join("settings")] {
            all &= private_dir(&d)?;
        }
        if all {
            let _ = self.checked.set(());
        }
        Ok(())
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

    fn settings_path(&self, url: &str) -> PathBuf {
        self.dir
            .join("settings")
            .join(format!("{}.json", digest(&[url.trim_end_matches('/')])))
    }

    /// Create the store's directories (see [`create_dir`]) and check them.
    fn create_dirs(&self) -> Result<()> {
        self.check()?;
        create_dir(&self.dir)?;
        create_dir(&self.urls_dir())?;
        create_dir(&self.dir.join("settings"))?;
        self.check()
    }

    /// [`read_json`], once the store is checked.
    fn read<T: DeserializeOwned>(&self, path: &Path) -> Result<Option<T>> {
        self.check()?;
        read_json(path)
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
        self.read(&self.token_path(issuer, client_id))
    }

    pub fn save_token(&self, t: &StoredToken) -> Result<()> {
        self.write(&self.token_path(&t.issuer, &t.client_id), t)
    }

    pub fn remove_token(&self, issuer: &str, client_id: &str) -> Result<bool> {
        self.check()?;
        remove(&self.token_path(issuer, client_id))
    }

    /// Record that `url`, advertising `cfg`, was logged in to with the
    /// provider login of `cfg`: its token may be sent there, and `cfg` is
    /// used for it until it rejects a request.
    pub fn record_url(&self, url: &str, cfg: &AuthConfig) -> Result<()> {
        let r = UrlLogin {
            url: url.trim_end_matches('/').into(),
            issuer: cfg.issuer.clone(),
            client_id: cfg.client_id.clone(),
            scopes: cfg.scopes.clone(),
            anonymous: cfg.anonymous,
        };
        self.write(&self.url_path(url), &r)
    }

    /// The provider login `url` was logged in with, if any.
    fn url_login(&self, url: &str) -> Result<Option<UrlLogin>> {
        let r: Option<UrlLogin> = self.read(&self.url_path(url))?;
        Ok(r.filter(|r| r.url == url.trim_end_matches('/')))
    }

    /// Cache the settings `url` advertises (`None`: no OIDC). This never
    /// makes it receive a token: only [`TokenStore::record_url`] does.
    fn save_settings(&self, url: &str, cfg: Option<&AuthConfig>) -> Result<()> {
        let s = UrlSettings {
            url: url.trim_end_matches('/').into(),
            config: cfg.cloned(),
        };
        self.write(&self.settings_path(url), &s)
    }

    /// The cached settings of `url`: `None` if not cached (or unreadable),
    /// `Some(None)` if it has no OIDC.
    fn settings(&self, url: &str) -> Option<Option<AuthConfig>> {
        let s: UrlSettings = self.read(&self.settings_path(url)).ok().flatten()?;
        (s.url == url.trim_end_matches('/')).then_some(s.config)
    }

    /// The URLs logged in to with (`issuer`, `client_id`).
    fn urls(&self, issuer: &str, client_id: &str) -> Result<Vec<String>> {
        self.check()?;
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
    /// The fixed token of a listener without OIDC (its [`Whoami`] names
    /// [`nfi_auth::TOKEN_USERNAME`]).
    StaticToken(Whoami),
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
    /// The service's settings, fetched on first use, by `login` or after a
    /// 401.
    discovered: OnceCell<Option<AuthConfig>>,
    current: Mutex<Option<String>>,
    noted_expiry: AtomicBool,
    allow_insecure: bool,
}

impl Credentials {
    /// `tool` is the command to suggest (`{tool} login`).
    pub fn new(tool: &'static str, url: &str, explicit: Option<String>, token_hint: &'static str) -> Self {
        Credentials {
            tool,
            url: url.trim_end_matches('/').to_string(),
            explicit,
            token_hint,
            store: TokenStore::default_location(),
            http: client(reqwest::redirect::Policy::default()),
            idp: client(reqwest::redirect::Policy::none()),
            discovered: OnceCell::new(),
            current: Mutex::new(None),
            noted_expiry: AtomicBool::new(false),
            allow_insecure: false,
        }
    }

    /// Allow sending tokens and logging in over plain HTTP to hosts other
    /// than loopback (`--allow-insecure`).
    pub fn allow_insecure(mut self, allow: bool) -> Self {
        self.allow_insecure = allow;
        self
    }

    /// Use `store` instead of the default token cache.
    pub fn with_store(mut self, store: TokenStore) -> Self {
        self.store = Some(store);
        self
    }

    /// The service's auth settings, fetched from it (once per process) and
    /// cached; `None` if it has no OIDC.
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
                let c = match r.status() {
                    s if s.is_success() => Some(r.json().await.with_context(|| format!("decoding {u}"))?),
                    reqwest::StatusCode::NOT_FOUND => None,
                    s => bail!("fetching {u}: HTTP {s}"),
                };
                if let Some(store) = &self.store {
                    store.save_settings(&self.url, c.as_ref())?;
                }
                Ok(c)
            })
            .await?;
        Ok(c.as_ref())
    }

    /// The token for a command: the explicit one, else the login of a URL
    /// logged in to (refreshed if needed). Without a usable one, a service
    /// without OIDC or accepting anonymous requests (as its settings say:
    /// those recorded, else cached, else fetched now) gets none; otherwise
    /// this logs in (if `prompt` and stderr is a terminal: see
    /// [`Credentials::reuse`]) or fails with the hint.
    pub async fn token(&self, prompt: bool) -> Result<Option<String>> {
        if let Some(t) = self.explicit_token()? {
            return Ok(Some(t));
        }
        let (cfg, expired) = match self.recorded()? {
            Some(cfg) => match self.stored(&cfg).await? {
                Login::Usable(t) => return Ok(Some(t)),
                Login::Missing => (cfg, false),
                Login::Expired => (cfg, true),
            },
            None => match self.settings().await? {
                Some(cfg) => (cfg, false),
                None => return Ok(None),
            },
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
        if prompt
            && interactive()
            && let Some(store) = &self.store
        {
            let Some(cfg) = self.config().await? else {
                return Ok(None);
            };
            if let Some((t, _)) = self.reuse(store, cfg).await? {
                return Ok(Some(t));
            }
            return self.device(cfg).await.map(Some);
        }
        bail!("{}", self.hint())
    }

    /// A usable login if there is one (refreshing it), never prompting or
    /// failing for lack of one (for `whoami`).
    pub async fn current(&self) -> Result<Option<String>> {
        if let Some(t) = self.explicit_token()? {
            return Ok(Some(t));
        }
        let Some(cfg) = self.recorded()? else {
            return Ok(None);
        };
        Ok(match self.stored(&cfg).await? {
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

    /// Log in: unless `force`, with the cached provider login if possible
    /// (see [`Credentials::reuse`]); otherwise run the device flow. Either
    /// way the URL is recorded as logged in. Returns who the service sees.
    pub async fn login(&self, force: bool) -> Result<Whoami> {
        let cfg = self
            .config()
            .await?
            .with_context(|| format!("{} does not use OIDC login", printable(&self.url)))?;
        self.secure(&self.url)?;
        if !force
            && let Some(store) = &self.store
            && let Some((_, w)) = self.reuse(store, cfg).await?
        {
            return Ok(w);
        }
        let t = self.device(cfg).await?;
        let r = self.ask(Some(&t)).await;
        if let Ok(Identity::User(w)) = r {
            return Ok(w);
        }
        // Keep the provider login, but stop sending it here.
        *self.current.lock().unwrap() = None;
        if let Some(store) = &self.store {
            remove(&store.url_path(&self.url))?;
        }
        let rejected = format!("{} rejected the new login", printable(&self.url));
        match r {
            Err(e) => Err(e.context(rejected)),
            Ok(_) => bail!("{rejected}"),
        }
    }

    /// Log in with the cached provider login for `cfg`, if there is one:
    /// used without asking if this URL was logged in with it, else only if
    /// the user agrees to use the login made for other URLs here (asked on
    /// the terminal; without one, this fails rather than ask). Records the
    /// URL if the service accepts it. `None`: run the device flow (no such
    /// login, the user said no, or it cannot be used: with a warning).
    async fn reuse(&self, store: &TokenStore, cfg: &AuthConfig) -> Result<Option<(String, Whoami)>> {
        let urls = store.urls(&cfg.issuer, &cfg.client_id)?;
        if urls.is_empty() || store.token(&cfg.issuer, &cfg.client_id)?.is_none() {
            return Ok(None);
        }
        if !urls.contains(&self.url) {
            let others = urls.iter().map(|u| printable(u)).collect::<Vec<_>>().join(", ");
            let url = printable(&self.url);
            match confirm(format!("Use your login for {others} to log in to {url}? [y/N] ")).await? {
                Some(true) => {}
                Some(false) => return Ok(None),
                None => bail!(
                    "the login for {others} could be used for {url}: run `{0} login` on a terminal \
                     to choose, or `{0} login --force` for a new login",
                    self.tool
                ),
            }
        }
        match self.accepted(store, cfg).await {
            Ok(Some((t, w))) => {
                store.record_url(&self.url, cfg)?;
                Ok(Some((self.remember(t), w)))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                eprintln!("warning: not using the cached login: {e:#}");
                Ok(None)
            }
        }
    }

    /// The cached provider login for `cfg` and who the service sees with
    /// it, if it accepts it.
    async fn accepted(&self, store: &TokenStore, cfg: &AuthConfig) -> Result<Option<(String, Whoami)>> {
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

    /// After a 401: forget the token in use, fetch the service's settings
    /// (they may have changed) and log in again if that is possible (no
    /// explicit token, a token cache, OIDC, stderr a terminal), recording
    /// the URL with the new settings.
    pub async fn relogin(&self) -> Result<Option<String>> {
        *self.current.lock().unwrap() = None;
        if self.explicit.is_some() {
            return Ok(None);
        }
        match self.config().await? {
            Some(cfg) if self.store.is_some() && interactive() => self.device(cfg).await.map(Some),
            _ => Ok(None),
        }
    }

    /// What to do about a 401. Suggests logging in if the service uses OIDC
    /// as far as known (from its settings fetched in this process, else its
    /// record, else its cached settings).
    pub fn hint(&self) -> String {
        if self.explicit.is_some() {
            return "the token was rejected".into();
        }
        let oidc = match self.discovered.get() {
            Some(c) => c.is_some(),
            None => self.store.as_ref().is_some_and(|s| {
                s.url_login(&self.url).ok().flatten().is_some()
                    || matches!(s.settings(&self.url), Some(Some(_)))
            }),
        };
        match (oidc, &self.store) {
            (true, Some(_)) => format!(
                "authentication required: run `{} login` or {}",
                self.tool, self.token_hint
            ),
            (true, None) => format!(
                "authentication required: {NO_CACHE_HINT} to log in, or {}",
                self.token_hint
            ),
            (false, _) => format!("authentication required: {}", self.token_hint),
        }
    }

    /// The explicit token, if any, unless it may not be sent to the URL.
    fn explicit_token(&self) -> Result<Option<String>> {
        if self.explicit.is_some() {
            self.secure(&self.url)?;
        }
        Ok(self.explicit.clone())
    }

    /// Fail unless credentials may be sent to `url` (see [`insecure`]).
    fn secure(&self, url: &str) -> Result<()> {
        if insecure(url) && !self.allow_insecure {
            bail!(
                "refusing to use credentials with {url} over plain HTTP: use https, or pass \
                 --allow-insecure or set NFI_ALLOW_INSECURE=1"
            );
        }
        Ok(())
    }

    /// The settings recorded for this URL when it was logged in to, if it
    /// was.
    fn recorded(&self) -> Result<Option<AuthConfig>> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        Ok(store.url_login(&self.url)?.map(UrlLogin::config))
    }

    /// The service's settings as cached, else fetched now (and cached).
    async fn settings(&self) -> Result<Option<AuthConfig>> {
        if let Some(c) = self.store.as_ref().and_then(|s| s.settings(&self.url)) {
            return Ok(c);
        }
        Ok(self.config().await?.cloned())
    }

    fn remember(&self, t: String) -> String {
        *self.current.lock().unwrap() = Some(t.clone());
        t
    }

    /// The login held in memory or the cache for this URL, given the
    /// settings recorded for it (so with the provider login it was logged in
    /// to with: a service merely claiming the same provider never gets it),
    /// refreshing it if needed.
    async fn stored(&self, cfg: &AuthConfig) -> Result<Login> {
        let held = self.current.lock().unwrap().clone();
        let l = match (held, &self.store) {
            (Some(t), _) if fresh(&t) => Login::Usable(t),
            (_, None) => Login::Missing,
            (_, Some(store)) => self.provider(store, cfg).await?,
        };
        if let Login::Usable(t) = &l {
            self.secure(&self.url)?;
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
            .with_context(|| format!("no token cache: {NO_CACHE_HINT}"))?;
        let t = self.device_login(cfg).await?;
        let _lock = self.lock(store, &cfg.issuer, &cfg.client_id).await?;
        store.save_token(&t)?;
        store.record_url(&self.url, cfg)?;
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
            _ => {
                let w: Whoami = http::check(r)
                    .await?
                    .json()
                    .await
                    .with_context(|| format!("decoding {u}"))?;
                let token = w.username == nfi_auth::TOKEN_USERNAME
                    && w.session.is_none()
                    && w.additional_info.is_empty();
                if token {
                    Identity::StaticToken(w)
                } else {
                    Identity::User(w)
                }
            }
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
        self.secure(&self.url)?;
        self.secure(&cfg.issuer)?;
        let md = self.metadata(&cfg.issuer).await?;
        self.secure(&md.token_endpoint)?;
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
        self.secure(&self.url)?;
        self.secure(&cfg.issuer)?;
        let md = self.metadata(&cfg.issuer).await?;
        let Some(dev_ep) = &md.device_authorization_endpoint else {
            bail!("{} does not support the device authorization grant", cfg.issuer);
        };
        self.secure(dev_ep)?;
        self.secure(&md.token_endpoint)?;
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

/// Ask a yes/no question on the controlling terminal (never stdin, which
/// may be data); `None` without one or unless [`interactive`].
async fn confirm(question: String) -> Result<Option<bool>> {
    if !interactive() {
        return Ok(None);
    }
    tokio::task::spawn_blocking(move || {
        use std::io::{BufRead, Write};
        let Ok(tty) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
        else {
            return Ok(None);
        };
        (&tty).write_all(question.as_bytes())?;
        let mut answer = String::new();
        std::io::BufReader::new(&tty).read_line(&mut answer)?;
        let a = answer.trim();
        Ok(Some(a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes")))
    })
    .await?
}

/// `s` with control characters escaped, to print a URL from a cache file or
/// a service.
fn printable(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// Whether credentials sent to `url` would cross the network unencrypted:
/// plain `http://` to a host other than loopback (`localhost`,
/// 127.0.0.0/8, `::1`).
fn insecure(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return false;
    };
    let host = u.host_str().unwrap_or_default();
    let loopback = host == "localhost"
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|a| a.is_loopback());
    u.scheme() == "http" && !loopback
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
        let cfg = |client_id: &str| AuthConfig {
            issuer: "i".into(),
            client_id: client_id.into(),
            scopes: vec!["openid".into()],
            anonymous: true,
        };
        s.record_url("http://a/", &cfg("c")).unwrap();
        s.record_url("http://b", &cfg("c")).unwrap();
        s.record_url("http://c", &cfg("other")).unwrap();
        let recorded = |u: &str| s.url_login(u).unwrap().map(UrlLogin::config);
        assert_eq!(recorded("http://a"), Some(cfg("c")));
        assert_eq!(recorded("http://c"), Some(cfg("other")));
        assert_eq!(recorded("http://d"), None);
        assert_eq!(s.urls("i", "c").unwrap(), ["http://a", "http://b"]);
        // A record in another format counts as missing.
        std::fs::write(
            s.url_path("http://e"),
            r#"{"url": "http://e", "issuer": "i", "client_id": "c"}"#,
        )
        .unwrap();
        assert!(s.url_login("http://e").unwrap().is_none());
        assert_eq!(s.urls("i", "c").unwrap(), ["http://a", "http://b"]);

        // Cached settings, also of no OIDC, apart from the URL records.
        s.save_settings("http://f/", Some(&cfg("c"))).unwrap();
        s.save_settings("http://g", None).unwrap();
        assert_eq!(s.settings("http://f"), Some(Some(cfg("c"))));
        assert_eq!(s.settings("http://g"), Some(None));
        assert_eq!(s.settings("http://h"), None);
        assert!(s.url_login("http://f").unwrap().is_none());
        assert_eq!(s.urls("i", "c").unwrap(), ["http://a", "http://b"]);

        // A corrupt file counts as missing.
        std::fs::write(s.token_path("i", "c"), "{").unwrap();
        assert!(s.token("i", "c").unwrap().is_none());
        std::fs::write(s.url_path("http://a"), "").unwrap();
        assert!(s.url_login("http://a").unwrap().is_none());
        std::fs::write(s.settings_path("http://f"), "{").unwrap();
        assert_eq!(s.settings("http://f"), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = s.settings_path("http://g");
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
            for dir in [s.dir.clone(), s.urls_dir(), p.parent().unwrap().into()] {
                assert_eq!(
                    std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
        }
    }

    /// The store's directories are created accessible by the owner only.
    #[cfg(unix)]
    #[test]
    fn store_dirs_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let s = TokenStore::at(d.path().join("t"));
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let cfg = AuthConfig {
            issuer: "i".into(),
            client_id: "c".into(),
            scopes: vec![],
            anonymous: false,
        };
        s.record_url("http://a", &cfg).unwrap();
        assert_eq!(
            (
                mode(s.dir.clone()),
                mode(s.urls_dir()),
                mode(s.dir.join("settings"))
            ),
            (0o700, 0o700, 0o700)
        );
    }

    /// Directories it did not create are used only if private to the user
    /// (and are not changed).
    #[cfg(unix)]
    #[test]
    fn store_refuses_shared_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("t");
        let chmod =
            |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
        let t = StoredToken {
            issuer: "i".into(),
            client_id: "c".into(),
            id_token: "x".into(),
            refresh_token: None,
        };
        let err = |s: &TokenStore| format!("{:#}", s.token("i", "c").unwrap_err());
        std::fs::create_dir(&dir).unwrap();
        chmod(&dir, 0o755);
        let s = TokenStore::at(&dir);
        let e = err(&s);
        assert!(
            e.contains(&format!("refusing to use the token cache {}", dir.display()))
                && e.contains("others have access to it (mode 755)"),
            "{e}"
        );
        assert!(s.save_token(&t).is_err());
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!dir.join("urls").exists());

        chmod(&dir, 0o700);
        s.save_token(&t).unwrap();
        // A subdirectory too, on a store's first use.
        chmod(&dir.join("settings"), 0o750);
        let s = TokenStore::at(&dir);
        assert!(err(&s).contains("mode 750"), "{}", err(&s));
        chmod(&dir.join("settings"), 0o700);
        assert!(s.token("i", "c").unwrap().is_some());

        // Owned by someone else (only root can arrange that).
        if unsafe { libc::geteuid() } == 0 {
            std::os::unix::fs::chown(&dir, Some(1), None).unwrap();
            let e = err(&TokenStore::at(&dir));
            assert!(e.contains("it is owned by uid 1, not by you (uid 0)"), "{e}");
        }
    }

    #[test]
    fn printable_urls() {
        assert_eq!(printable("http://a/\x1b[2Jb\n"), "http://a/\\u{1b}[2Jb\\n");
        assert_eq!(printable("https://n\u{e9}.example"), "https://n\u{e9}.example");
    }

    #[test]
    fn insecure_urls() {
        for u in [
            "http://192.0.2.1",
            "http://nfi.example:8080/x",
            "http://[2001:db8::1]",
        ] {
            assert!(insecure(u), "{u}");
        }
        for u in [
            "https://nfi.example",
            "http://localhost:8080",
            "http://127.0.0.1",
            "http://127.1.2.3:9",
            "http://[::1]:80",
        ] {
            assert!(!insecure(u), "{u}");
        }
        let c = Credentials::new("t", "http://192.0.2.1/", Some("x".into()), "");
        let e = c.explicit_token().unwrap_err().to_string();
        assert!(
            e.contains("http://192.0.2.1")
                && e.contains("--allow-insecure")
                && e.contains("NFI_ALLOW_INSECURE=1"),
            "{e}"
        );
        let c = c.allow_insecure(true);
        assert_eq!(c.explicit_token().unwrap().as_deref(), Some("x"));
        let c = Credentials::new("t", "https://nfi.example", Some("x".into()), "");
        assert!(c.explicit_token().is_ok());
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

//! Bearer tokens for the CLIs: an explicit token, else a cached OIDC login,
//! logging in interactively after a 401.

use anyhow::{Context, Result};
use nfi_api::client::{Client, HttpError};
use nfi_auth::Whoami;
use nfi_auth::client::{Session, TokenStore};

pub struct Login {
    url: String,
    explicit: Option<String>,
    /// `None` when there is nowhere to cache tokens (no HOME).
    session: Option<Session>,
    /// The command to suggest when authentication is needed.
    tool: &'static str,
}

impl Login {
    pub fn new(tool: &'static str, url: &str, explicit: Option<String>) -> Result<Self> {
        let session = match TokenStore::default_location() {
            Ok(s) => Some(Session::new(s)?),
            Err(_) => None,
        };
        Ok(Login {
            url: url.to_string(),
            explicit,
            session,
            tool,
        })
    }

    fn session(&self) -> Result<&Session> {
        self.session
            .as_ref()
            .context("no token cache: set HOME, XDG_CACHE_HOME or NFI_TOKEN_CACHE")
    }

    /// A client carrying the explicit token or a cached login, if any.
    pub async fn client(&self) -> Result<Client> {
        let token = match &self.session {
            Some(s) => s.bearer(&self.url, self.explicit.clone()).await?,
            None => self.explicit.clone(),
        };
        Client::new(&self.url, token)
    }

    /// Run `f`; if the service answers 401 and no explicit token was given,
    /// log in interactively (when possible) and run it once more.
    pub async fn run<T, F, Fut>(&self, mut f: F) -> Result<T>
    where
        F: FnMut(Client) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let client = self.client().await?;
        let sent = client.has_token();
        let e = match f(client).await {
            Err(e) if HttpError::is_unauthorized(&e) && self.explicit.is_none() => e,
            r => return r,
        };
        let token = match &self.session {
            Some(s) => s.relogin(&self.url, sent).await?,
            None => None,
        };
        match token {
            Some(t) => f(Client::new(&self.url, Some(t))?).await,
            None => Err(e.context(format!("authentication required: run `{} login`", self.tool))),
        }
    }

    /// Log in (reusing a cached login for the same provider unless `force`).
    pub async fn login(&self, force: bool) -> Result<Whoami> {
        let (_, token) = self
            .session()?
            .login(&self.url, force, |p| eprintln!("{p}"))
            .await?;
        Client::new(&self.url, Some(token))?
            .get_json(nfi_auth::WHOAMI_PATH)
            .await
    }

    /// Forget the login. Returns whether there was one.
    pub async fn logout(&self) -> Result<bool> {
        self.session()?.logout(&self.url).await
    }

    pub async fn whoami(&self) -> Result<Whoami> {
        self.run(|c| async move { c.get_json(nfi_auth::WHOAMI_PATH).await })
            .await
    }
}

/// `alice` or `alice (session s1)`.
pub fn describe(w: &Whoami) -> String {
    match &w.session {
        Some(s) => format!("{} (session {s})", w.username),
        None => w.username.clone(),
    }
}

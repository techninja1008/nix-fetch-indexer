//! Tokens for the yoink and NFI services.

use anyhow::Result;
use nfi_auth::client::{Session, TokenStore};
use std::future::Future;

pub const YOINK_HINT: &str = "run `yoink login` or set YOINK_TOKEN";
pub const NFI_HINT: &str = "run `yoink login` (with the NFI URL configured) or set NFI_TOKEN";

/// The login cache, or `None` when there is nowhere to keep it (no HOME):
/// explicit tokens still work then.
pub fn session() -> Result<Option<Session>> {
    match TokenStore::default_location() {
        Ok(s) => Ok(Some(Session::new(s)?)),
        Err(_) => Ok(None),
    }
}

/// The token to send: `explicit` if given, else a cached login.
pub async fn bearer(
    session: Option<&Session>,
    url: &str,
    explicit: Option<String>,
) -> Result<Option<String>> {
    match session {
        Some(s) => s.bearer(url, explicit).await,
        None => Ok(explicit),
    }
}

/// Whether a request failed with HTTP 401.
pub fn is_unauthorized(e: &anyhow::Error) -> bool {
    nfi_api::client::HttpError::is_unauthorized(e)
        || e.chain().any(|c| {
            c.downcast_ref::<yoink_api::client::HttpError>()
                .is_some_and(|h| h.status == 401)
        })
}

/// Run `f` with `token`; on a 401, unless the token was `explicit`, log in
/// interactively once (if possible) and run it again with the new token,
/// which is stored in `token`.
pub async fn with_login<T, F, Fut>(
    session: Option<&Session>,
    url: &str,
    token: &mut Option<String>,
    explicit: bool,
    hint: &str,
    f: F,
) -> Result<T>
where
    F: Fn(Option<String>) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let e = match f(token.clone()).await {
        Err(e) if is_unauthorized(&e) => e,
        r => return r,
    };
    let t = match session {
        Some(s) if !explicit => s.relogin(url, token.is_some()).await?,
        _ => None,
    };
    match t {
        Some(t) => {
            *token = Some(t);
            f(token.clone()).await
        }
        None => Err(e.context(format!("{url} requires authentication; {hint}"))),
    }
}

//! Tokens for the yoink and NFI services.

use anyhow::Result;
use nfi_auth::client::{Session, read_token_file};
use std::future::Future;
use std::path::Path;

/// A token given explicitly: from `file`, else the environment variable `env`.
pub fn explicit(file: Option<&Path>, env: &str) -> Result<Option<String>> {
    if let Some(f) = file {
        return Ok(Some(read_token_file(f)?));
    }
    Ok(std::env::var(env)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty()))
}

/// Whether a request failed with HTTP 401.
pub fn is_unauthorized(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<yoink_api::client::HttpError>()
            .is_some_and(|h| h.status == 401)
            || c.downcast_ref::<nfi_api::client::HttpError>()
                .is_some_and(|h| h.status == 401)
    })
}

/// Run `f` with `token`; on a 401, log in interactively once (if possible)
/// and run it again with the new token, which is stored in `token`.
pub async fn with_login<T, F, Fut>(
    session: &Session,
    url: &str,
    token: &mut Option<String>,
    hint: &str,
    f: F,
) -> Result<T>
where
    F: Fn(Option<String>) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    match f(token.clone()).await {
        Err(e) if is_unauthorized(&e) => match session.relogin(url, token.is_some()).await? {
            Some(t) => {
                *token = Some(t);
                f(token.clone()).await
            }
            None => Err(e.context(format!("{url} requires authentication; {hint}"))),
        },
        r => r,
    }
}

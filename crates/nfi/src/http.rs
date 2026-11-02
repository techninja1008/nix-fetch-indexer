//! HTTP errors shared by the nfi and yoink API clients, and retrying a
//! request after logging in again.

use anyhow::Result;
use nfi_api::ErrorBody;
use nfi_auth_client::Credentials;

/// A non-success HTTP response from the service.
#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match reqwest::StatusCode::from_u16(self.status) {
            Ok(s) => write!(f, "HTTP {s}: {}", self.message),
            Err(_) => write!(f, "HTTP {}: {}", self.status, self.message),
        }
    }
}

impl std::error::Error for HttpError {}

/// Whether `e` is (or wraps) a 401 response.
pub fn is_unauthorized(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| c.downcast_ref::<HttpError>().is_some_and(|h| h.status == 401))
}

/// `resp` if it is a success, otherwise an [`HttpError`] with the message
/// of its [`ErrorBody`] (or its body as text).
pub async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let bytes = resp.bytes().await.unwrap_or_default();
    let message = serde_json::from_slice::<ErrorBody>(&bytes)
        .map(|e| e.error)
        .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
    Err(HttpError {
        status: status.as_u16(),
        message,
    }
    .into())
}

/// Run one request with the current token; on a 401, unless the token was
/// explicit, log in again if `prompt` (and stderr is a terminal) and run it
/// once more. Wrap single requests only: a 401 means nothing was done.
pub async fn retry<T, F, Fut>(creds: &Credentials, prompt: bool, f: F) -> Result<T>
where
    F: Fn(Option<String>) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let e = match f(creds.token(prompt).await?).await {
        Err(e) if is_unauthorized(&e) => e,
        r => return r,
    };
    let token = if prompt { creds.relogin().await? } else { None };
    match token {
        Some(t) => f(Some(t)).await,
        None => Err(e.context(creds.hint())),
    }
}

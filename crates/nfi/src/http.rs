//! HTTP errors shared by the nfi and yoink API clients.

use anyhow::Result;
use nfi_api::ErrorBody;

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

//! OIDC device code authentication and CEL authorisation.
//!
//! Services validate OIDC ID tokens (JWTs) presented as bearer tokens,
//! extract a principal from their claims with CEL expressions, and authorise
//! requests with further CEL expressions ([`server`]). CLIs log in with the
//! device authorization grant and cache tokens per issuer and client
//! ([`client`]). Services advertise their OIDC settings at [`CONFIG_PATH`]
//! so that a CLI only needs the service URL to log in.

#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "testing")]
pub mod testing;

use serde::{Deserialize, Serialize};

/// Where a service advertises its [`AuthConfig`] (unauthenticated GET; 404
/// when OIDC is not configured on that listener).
pub const CONFIG_PATH: &str = "/auth/v1/config";
/// Where a service reports the caller's [`Whoami`] (401 without a valid token).
pub const WHOAMI_PATH: &str = "/auth/v1/whoami";

/// The OIDC settings a service advertises to its clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthConfig {
    /// OpenID provider issuer URL; its discovery document is at
    /// `<issuer>/.well-known/openid-configuration`.
    pub issuer: String,
    /// Public client to run the device authorization grant as. ID tokens
    /// must be issued to it (their `aud`).
    pub client_id: String,
    /// Scopes to request.
    pub scopes: Vec<String>,
}

/// The principal a service extracted from the caller's token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Whoami {
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
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
    #[test]
    fn expiry() {
        use base64::Engine;
        let enc = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        let t = format!("{}.{}.sig", enc("{}"), enc(r#"{"exp":1234,"sub":"x"}"#));
        assert_eq!(super::jwt_expiry(&t), Some(1234));
        assert_eq!(super::jwt_expiry("garbage"), None);
    }
}

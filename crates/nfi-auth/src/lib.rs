//! Authentication protocol types shared by the nfi and yoink services and
//! CLIs.
//!
//! Services validate OIDC ID tokens (JWTs) presented as bearer tokens,
//! extract a principal from their claims with CEL expressions, and authorise
//! requests with further CEL expressions (`nfi-auth-server`). CLIs log in
//! with the device authorization grant and cache tokens per issuer and
//! client (`nfi-auth-client`). Services advertise their OIDC settings at
//! [`CONFIG_PATH`] so that a CLI only needs the service URL to log in.

use serde::{Deserialize, Serialize};

/// Where a service advertises its [`AuthConfig`] (unauthenticated GET; 404
/// when OIDC is not configured on that listener).
pub const CONFIG_PATH: &str = "/auth/v1/config";
/// Where a service reports the caller's [`Whoami`] (401 without a valid
/// token; 404 when OIDC is not configured on that listener).
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
    /// Whether the service also serves requests without a token.
    #[serde(default)]
    pub anonymous: bool,
}

/// The principal a service extracted from the caller's token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Whoami {
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// What the service's whoami expression made of the token (by default
    /// its claims); empty from services that predate it.
    #[serde(default)]
    pub additional_info: serde_json::Map<String, serde_json::Value>,
}

/// The JSON body of an error response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

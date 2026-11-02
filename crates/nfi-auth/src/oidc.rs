//! OpenID provider protocol types: the discovery document (OpenID Connect
//! Discovery), the device authorization grant (RFC 8628) and token
//! endpoint responses (RFC 6749).

use serde::{Deserialize, Serialize};

/// The provider's discovery document, at
/// `<issuer>/.well-known/openid-configuration` (the fields used here).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderMetadata {
    pub issuer: String,
    pub jwks_uri: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_authorization_endpoint: Option<String>,
}

/// A device authorization response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    #[serde(alias = "verification_url")]
    pub verification_uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_uri_complete: Option<String>,
    /// Seconds until the codes expire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    /// Seconds to wait between token requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<u64>,
}

/// A successful token response (the fields used here).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

/// An OAuth error response, e.g. `authorization_pending` or
/// `invalid_grant`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenError {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
}

//! Authentication and authorisation of the public and admin listeners.
//!
//! - Public API: open unless OIDC is configured. With OIDC, a bearer token
//!   must be a valid ID token that passes the main policy; requests without
//!   one are let through (skipping the policy) only with anonymous access.
//! - Admin API: open, a fixed bearer token, or OIDC with the admin policy.
//!
//! Policies are CEL expressions over `claims`, `user` and `request`
//! (`{"method", "path"}`); see `nfi_auth_server`.

use nfi_auth_server::{Authenticator, Expr, Guard};
use std::sync::Arc;

/// The guards of the two listeners.
pub struct ServiceAuth {
    pub public: Arc<Guard>,
    pub admin: Arc<Guard>,
}

/// How the admin listener authenticates callers when the service uses OIDC.
pub enum AdminAuth {
    /// No authentication (keep the listener on a private interface).
    Open,
    /// A fixed bearer token.
    Token(String),
    /// OIDC ID tokens that pass this policy.
    Oidc(Expr),
}

impl AdminAuth {
    fn guard(self, authenticator: &Arc<Authenticator>) -> Guard {
        match self {
            AdminAuth::Open => Guard::Open,
            AdminAuth::Token(t) => Guard::Token(t),
            AdminAuth::Oidc(policy) => Guard::Oidc {
                authenticator: authenticator.clone(),
                policy,
                anonymous: false,
            },
        }
    }
}

impl ServiceAuth {
    /// No authentication anywhere.
    pub fn open() -> Self {
        Self::without_oidc(Guard::Open)
    }

    /// An open public API and an admin listener protected by `token`.
    pub fn admin_token(token: &str) -> Self {
        Self::without_oidc(Guard::Token(token.to_string()))
    }

    fn without_oidc(admin: Guard) -> Self {
        ServiceAuth {
            public: Arc::new(Guard::Open),
            admin: Arc::new(admin),
        }
    }

    /// OIDC on the public API with the `main` policy (and anonymous access
    /// if `anonymous`), and `admin` on the admin listener.
    pub fn oidc(authenticator: Arc<Authenticator>, main: Expr, anonymous: bool, admin: AdminAuth) -> Self {
        ServiceAuth {
            admin: Arc::new(admin.guard(&authenticator)),
            public: Arc::new(Guard::Oidc {
                authenticator,
                policy: main,
                anonymous,
            }),
        }
    }
}

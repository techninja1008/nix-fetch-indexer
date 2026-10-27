//! Authentication and authorisation of the public and admin listeners.
//!
//! - Public API: open unless OIDC is configured. With OIDC, a bearer token
//!   must be a valid ID token that passes the main policy; requests without
//!   one are let through (skipping the policy) only with `allow_anonymous`.
//! - Admin API: open, a fixed bearer token, or OIDC with the admin policy.
//!
//! Policies are CEL expressions over `claims`, `user` and `request`
//! (`{"method", "path"}`); see `nfi_auth::server`.

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use nfi_api::ErrorBody;
use nfi_auth::server::{AuthError, Authenticator, Expr, Principal, Vars, bearer, request_var, token_matches};
use std::sync::Arc;
use tracing::Instrument;

/// How the admin listener authenticates callers.
pub enum AdminAuth {
    /// No authentication (keep the listener on a private interface).
    Open,
    /// A fixed bearer token.
    Token(String),
    /// OIDC ID tokens that pass this policy.
    Oidc(Expr),
}

pub struct ServiceAuth {
    pub oidc: Option<Authenticator>,
    /// Let requests without a token use the public API (OIDC only).
    pub allow_anonymous: bool,
    /// Policy for authenticated public API requests.
    pub main_authz: Expr,
    pub admin: AdminAuth,
}

impl ServiceAuth {
    /// No authentication anywhere.
    pub fn open() -> Self {
        Self::with_admin(AdminAuth::Open)
    }

    /// An open public API and an admin listener protected by `token`.
    pub fn admin_token(token: &str) -> Self {
        Self::with_admin(AdminAuth::Token(token.to_string()))
    }

    fn with_admin(admin: AdminAuth) -> Self {
        ServiceAuth {
            oidc: None,
            allow_anonymous: false,
            main_authz: Expr::new("true").expect("valid expression"),
            admin,
        }
    }
}

/// Who made a request; a request extension set by the auth middleware.
#[derive(Clone, Debug)]
pub enum Caller {
    Anonymous,
    AdminToken,
    User(Principal),
}

fn auth_error(e: AuthError) -> Response {
    let status = e.status();
    if !matches!(e, AuthError::Internal(_)) {
        tracing::info!(status = status.as_u16(), error = %e, "request rejected");
    } else {
        tracing::error!(error = %e, "authentication failed");
    }
    let mut r = (status, Json(ErrorBody { error: e.to_string() })).into_response();
    if status == StatusCode::UNAUTHORIZED {
        r.headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    r
}

/// Validate the bearer token of `req` with OIDC and apply `policy`.
fn oidc_caller<'a>(
    a: &'a Authenticator,
    policy: &'a Expr,
    req: &Request,
) -> impl Future<Output = Result<Principal, AuthError>> + Send + 'a {
    // Copy what is needed: the request (its body) is not `Sync`, so it
    // can't be borrowed across the await.
    let token = bearer(req.headers()).map(str::to_string);
    let request = request_var(req.method(), req.uri().path());
    async move { oidc_principal(a, policy, token, request).await }
}

async fn oidc_principal(
    a: &Authenticator,
    policy: &Expr,
    token: Option<String>,
    request: serde_json::Value,
) -> Result<Principal, AuthError> {
    let token = token.ok_or(AuthError::Missing)?;
    let p = a.authenticate(&token).await?;
    let mut v: Vars = p.vars();
    v.insert("request".into(), request);
    policy.authorize(&v)?;
    Ok(p)
}

/// Run the handler with the caller attached and recorded in the log.
async fn run_as(caller: Caller, mut req: Request, next: Next) -> Response {
    let (user, session) = match &caller {
        Caller::Anonymous => ("-".to_string(), None),
        Caller::AdminToken => ("(admin token)".to_string(), None),
        Caller::User(p) => (p.username.clone(), p.session.clone()),
    };
    let span = tracing::info_span!(
        "request",
        method = %req.method(),
        path = %req.uri().path(),
        user = %user,
        session = session.as_deref().unwrap_or("-"),
    );
    req.extensions_mut().insert(caller);
    async move {
        let r = next.run(req).await;
        tracing::info!(status = r.status().as_u16(), "request");
        r
    }
    .instrument(span)
    .await
}

/// Routes that never need authentication.
fn exempt(req: &Request) -> bool {
    matches!(req.uri().path(), "/healthz" | nfi_auth::CONFIG_PATH)
}

pub async fn public_auth(State(auth): State<Arc<ServiceAuth>>, req: Request, next: Next) -> Response {
    let Some(a) = &auth.oidc else {
        return next.run(req).await;
    };
    if exempt(&req) {
        return next.run(req).await;
    }
    let caller = if bearer(req.headers()).is_none() && auth.allow_anonymous {
        Caller::Anonymous
    } else {
        match oidc_caller(a, &auth.main_authz, &req).await {
            Ok(p) => Caller::User(p),
            Err(e) => return auth_error(e),
        }
    };
    run_as(caller, req, next).await
}

pub async fn admin_auth(State(auth): State<Arc<ServiceAuth>>, req: Request, next: Next) -> Response {
    if exempt(&req) {
        return next.run(req).await;
    }
    let caller = match &auth.admin {
        AdminAuth::Open => return next.run(req).await,
        AdminAuth::Token(token) => {
            if !bearer(req.headers()).is_some_and(|t| token_matches(t, token)) {
                return auth_error(AuthError::Invalid("missing or invalid admin token".into()));
            }
            Caller::AdminToken
        }
        AdminAuth::Oidc(policy) => {
            let a = auth.oidc.as_ref().expect("admin OIDC requires OIDC settings");
            match oidc_caller(a, policy, &req).await {
                Ok(p) => Caller::User(p),
                Err(e) => return auth_error(e),
            }
        }
    };
    run_as(caller, req, next).await
}

/// `CONFIG_PATH` on a listener that uses OIDC.
pub fn config_response(a: Option<&Authenticator>) -> Response {
    match a {
        Some(a) => Json(a.config()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorBody {
                error: "this listener does not use OIDC login".into(),
            }),
        )
            .into_response(),
    }
}

/// `WHOAMI_PATH`: the caller's principal, 401 if not logged in.
pub fn whoami_response(caller: Option<&Caller>) -> Response {
    match caller {
        Some(Caller::User(p)) => Json(p.whoami()).into_response(),
        _ => auth_error(AuthError::Invalid("not logged in".into())),
    }
}

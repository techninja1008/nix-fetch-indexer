//! Per-listener authentication: the [`Guard`], its axum [`middleware`],
//! and the responses of [`nfi_auth::CONFIG_PATH`] and
//! [`nfi_auth::WHOAMI_PATH`].
//!
//! Every request that passes the guard runs in a `request` span with
//! `method`, `path`, `user` and `session` (`-` for none) and ends with an
//! `info` line `request` carrying its `status`. A rejected request is
//! logged as `request rejected` with `method`, `path`, `status` and
//! `error`: at `info` level, or `error` for a 5xx.

use crate::{AuthError, Authenticator, Expr, Principal, Vars, bearer, request_var, token_matches};
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use nfi_auth::{AuthConfig, ErrorBody};
use std::sync::Arc;
use tracing::Instrument;

/// How one listener authenticates and authorises requests.
pub enum Guard {
    /// No authentication.
    Open,
    /// A fixed bearer token.
    Token(String),
    /// OIDC ID tokens that pass `policy` (vars: `claims`, `user`,
    /// `request`). With `anonymous`, requests without a token are let
    /// through without the policy.
    Oidc {
        authenticator: Arc<Authenticator>,
        policy: Expr,
        anonymous: bool,
    },
}

/// Who made a request; set as a request extension by [`middleware`].
#[derive(Clone, Debug)]
pub enum Caller {
    /// No credentials (an open listener, or anonymous access).
    Anonymous,
    /// The fixed token of a [`Guard::Token`].
    Token,
    /// An OIDC user, with the policy variables (`claims`, `user`,
    /// `request`) of this request.
    User { principal: Principal, vars: Vars },
}

impl Caller {
    /// `user` and `session` for the log.
    fn log_identity(&self) -> (&str, &str) {
        match self {
            Caller::Anonymous => ("-", "-"),
            Caller::Token => ("(token)", "-"),
            Caller::User { principal, .. } => {
                (&principal.username, principal.session.as_deref().unwrap_or("-"))
            }
        }
    }
}

fn not_oidc() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody {
            error: "this listener does not use OIDC login".into(),
        }),
    )
        .into_response()
}

impl Guard {
    /// Authenticate and authorise a request.
    pub async fn check(&self, h: &HeaderMap, m: &Method, path: &str) -> Result<Caller, AuthError> {
        match self {
            Guard::Open => Ok(Caller::Anonymous),
            Guard::Token(expected) => match bearer(h) {
                None => Err(AuthError::Missing),
                Some(t) if token_matches(t, expected) => Ok(Caller::Token),
                Some(_) => Err(AuthError::Invalid("wrong token".into())),
            },
            Guard::Oidc {
                authenticator,
                policy,
                anonymous,
            } => {
                let Some(token) = bearer(h) else {
                    return if *anonymous {
                        Ok(Caller::Anonymous)
                    } else {
                        Err(AuthError::Missing)
                    };
                };
                let principal = authenticator.authenticate(token).await?;
                let mut vars = principal.vars();
                vars.insert("request".into(), request_var(m, path));
                policy.authorize(&vars)?;
                Ok(Caller::User { principal, vars })
            }
        }
    }

    /// The [`nfi_auth::CONFIG_PATH`] response: the [`AuthConfig`], or 404
    /// unless this is an OIDC guard.
    pub fn config_response(&self) -> Response {
        match self {
            Guard::Oidc {
                authenticator,
                anonymous,
                ..
            } => Json(AuthConfig {
                anonymous: *anonymous,
                ..authenticator.config()
            })
            .into_response(),
            _ => not_oidc(),
        }
    }

    /// The [`nfi_auth::WHOAMI_PATH`] response for `caller`: 404 unless this
    /// is an OIDC guard, 401 unless the caller is a user, 500 if the whoami
    /// expression fails.
    pub fn whoami_response(&self, caller: Option<&Caller>) -> Response {
        let Guard::Oidc { authenticator, .. } = self else {
            return not_oidc();
        };
        let Some(Caller::User { principal, .. }) = caller else {
            return AuthError::Missing.into_response();
        };
        match authenticator.whoami(principal) {
            Ok(w) => Json(w).into_response(),
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "whoami expression failed");
                AuthError::Internal("whoami expression failed".into()).into_response()
            }
        }
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let status = self.status();
        let mut r = (
            status,
            Json(ErrorBody {
                error: self.to_string(),
            }),
        )
            .into_response();
        if status == StatusCode::UNAUTHORIZED {
            r.headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        r
    }
}

/// Check each request with the guard: rejected requests get the
/// [`AuthError`] response; the others run with their [`Caller`] as an
/// extension, in the `request` span (see the module docs).
pub async fn middleware(State(guard): State<Arc<Guard>>, mut req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let caller = match guard.check(req.headers(), &method, &path).await {
        Ok(c) => c,
        Err(e) => {
            let status = e.status().as_u16();
            if e.status().is_server_error() {
                tracing::error!(%method, path = %path, status, error = %e, "request rejected");
            } else {
                tracing::info!(%method, path = %path, status, error = %e, "request rejected");
            }
            return e.into_response();
        }
    };
    let (user, session) = caller.log_identity();
    let span = tracing::info_span!("request", %method, path = %path, user = %user, session = %session);
    req.extensions_mut().insert(caller);
    async move {
        let r = next.run(req).await;
        tracing::info!(status = r.status().as_u16(), "request");
        r
    }
    .instrument(span)
    .await
}

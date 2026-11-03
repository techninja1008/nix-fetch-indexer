//! Per-listener authentication: the [`Guard`], its axum [`middleware`],
//! and the unauthenticated [`Guard::routes`] ([`CONFIG_PATH`] and
//! [`WHOAMI_PATH`]).
//!
//! Every request that passes the guard (and every whoami request that
//! authenticates) runs in a `request` span with `method`, `path`, `user`
//! and `session` (`-` for none) and ends with an `info` line `request`
//! carrying its `status`. A rejected request is logged as `request
//! rejected` with `method`, `path`, `status` and `error`: at `info` level,
//! or `error` for a 5xx.

use crate::{AuthError, Authenticator, Expr, Principal, Vars, bearer, request_var, token_matches};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use nfi_auth::{AuthConfig, CONFIG_PATH, ErrorBody, TOKEN_USERNAME, WHOAMI_PATH, Whoami};
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
            Caller::Token => (TOKEN_USERNAME, "-"),
            Caller::User { principal, .. } => principal.log_identity(),
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

impl Principal {
    /// `user` and `session` for the log.
    fn log_identity(&self) -> (&str, &str) {
        (&self.username, self.session.as_deref().unwrap_or("-"))
    }
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
                vars.insert("request".into(), request_var(m, path)?);
                policy.authorize(&vars)?;
                Ok(Caller::User { principal, vars })
            }
        }
    }

    /// The [`CONFIG_PATH`] response: the [`AuthConfig`], or 404
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

    /// The [`WHOAMI_PATH`] response: 404 for an open guard; otherwise the
    /// caller's token is authenticated, without the policy or anonymous
    /// access: the [`AuthError`] response if that fails, 500 if the whoami
    /// expression fails, else 200 with the [`Whoami`] (for the fixed token
    /// of a token guard, [`TOKEN_USERNAME`]). Logged like requests through
    /// the [`middleware`].
    pub async fn whoami_response(&self, headers: &HeaderMap) -> Response {
        let (method, path) = (Method::GET, WHOAMI_PATH);
        let authenticator = match self {
            Guard::Open => return span(&method, path, ("-", "-")).in_scope(|| logged(not_oidc())),
            Guard::Token(_) => {
                return match self.check(headers, &method, path).await {
                    Ok(c) => span(&method, path, c.log_identity()).in_scope(|| {
                        logged(
                            Json(Whoami {
                                username: TOKEN_USERNAME.into(),
                                session: None,
                                additional_info: Default::default(),
                            })
                            .into_response(),
                        )
                    }),
                    Err(e) => rejected(&method, path, e),
                };
            }
            Guard::Oidc { authenticator, .. } => authenticator,
        };
        let principal = match bearer(headers) {
            None => Err(AuthError::Missing),
            Some(t) => authenticator.authenticate(t).await,
        };
        let principal = match principal {
            Ok(p) => p,
            Err(e) => return rejected(&method, path, e),
        };
        span(&method, path, principal.log_identity()).in_scope(|| {
            logged(match authenticator.whoami(&principal) {
                Ok(w) => Json(w).into_response(),
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "whoami expression failed");
                    AuthError::Internal("whoami expression failed".into()).into_response()
                }
            })
        })
    }

    /// The unauthenticated routes, [`CONFIG_PATH`] and [`WHOAMI_PATH`], to
    /// merge into a service's router outside the [`middleware`].
    pub fn routes<S: Clone + Send + Sync + 'static>(self: &Arc<Self>) -> Router<S> {
        let (g1, g2) = (self.clone(), self.clone());
        Router::new()
            .route(CONFIG_PATH, get(move || async move { g1.config_response() }))
            .route(
                WHOAMI_PATH,
                get(move |h: HeaderMap| async move { g2.whoami_response(&h).await }),
            )
    }
}

/// The `request` span of a request (see the module docs).
fn span(method: &Method, path: &str, (user, session): (&str, &str)) -> tracing::Span {
    tracing::info_span!("request", %method, path = %path, user = %user, session = %session)
}

/// Log the final `request` line of a response.
fn logged(r: Response) -> Response {
    tracing::info!(status = r.status().as_u16(), "request");
    r
}

/// Log a rejected request and respond with the error.
fn rejected(method: &Method, path: &str, e: AuthError) -> Response {
    let status = e.status().as_u16();
    if e.status().is_server_error() {
        tracing::error!(%method, path = %path, status, error = %e, "request rejected");
    } else {
        tracing::info!(%method, path = %path, status, error = %e, "request rejected");
    }
    e.into_response()
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
        Err(e) => return rejected(&method, &path, e),
    };
    let span = span(&method, &path, caller.log_identity());
    req.extensions_mut().insert(caller);
    async move { logged(next.run(req).await) }.instrument(span).await
}

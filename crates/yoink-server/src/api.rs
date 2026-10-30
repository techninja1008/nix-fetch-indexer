//! HTTP routes: auth discovery, whoami and the mirror stream.

use crate::job::Update;
use crate::{Auth, Mirror};
use axum::Json;
use axum::Router;
use axum::extract::{Request, State as AxState};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::{Stream, StreamExt};
use nfi_auth::server::{AuthError, Principal, Vars, bearer, request_var};
use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::Instrument;
use yoink_api::{ErrorBody, Event, LogLevel, MIRROR_PATH, MirrorRequest, Outcome, Purl};

/// Minimum interval between updates forwarded for one job, so fast progress
/// reports don't flood the stream.
const UPDATE_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub struct State {
    mirror: Arc<dyn Mirror>,
    auth: Arc<Auth>,
    max_purls: usize,
}

impl State {
    pub fn new(mirror: Arc<dyn Mirror>, auth: Auth, max_purls: usize) -> Self {
        State {
            mirror,
            auth: Arc::new(auth),
            max_purls,
        }
    }
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    let mut r = (status, Json(ErrorBody { error: msg.into() })).into_response();
    if status == StatusCode::UNAUTHORIZED {
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer"),
        );
    }
    r
}

fn auth_error(e: AuthError) -> Response {
    error(e.status(), e.to_string())
}

pub fn router(state: State) -> Router {
    Router::new()
        .route(MIRROR_PATH, post(mirror))
        .route(nfi_auth::WHOAMI_PATH, get(whoami))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .route("/healthz", get(|| async { "ok" }))
        .route(nfi_auth::CONFIG_PATH, get(config))
        .with_state(state)
}

/// The authenticated caller and the request-level policy variables.
#[derive(Clone)]
struct Caller {
    principal: Principal,
    vars: Vars,
}

async fn authenticate(AxState(s): AxState<State>, mut req: Request, next: Next) -> Response {
    let Some(token) = bearer(req.headers()) else {
        return auth_error(AuthError::Missing);
    };
    let principal = match s.auth.authenticator.authenticate(token).await {
        Ok(p) => p,
        Err(e) => {
            let (method, path) = (req.method(), req.uri().path());
            if matches!(e, AuthError::Internal(_)) {
                tracing::error!(%method, path, error = %e, "authentication failed");
            } else {
                tracing::info!(%method, path, error = %e, "rejected request");
            }
            return auth_error(e);
        }
    };
    let mut vars = principal.vars();
    vars.insert("request".into(), request_var(req.method(), req.uri().path()));
    let span = tracing::info_span!(
        "request",
        user = %principal.username,
        session = principal.session.as_deref().unwrap_or(""),
        method = %req.method(),
        path = req.uri().path(),
    );
    if let Err(e) = s.auth.request.authorize(&vars) {
        span.in_scope(|| tracing::info!(error = %e, "forbidden"));
        return auth_error(e);
    }
    req.extensions_mut().insert(Caller { principal, vars });
    next.run(req).instrument(span).await
}

async fn config(AxState(s): AxState<State>) -> Response {
    Json(s.auth.authenticator.config()).into_response()
}

async fn whoami(req: Request) -> Response {
    match req.extensions().get::<Caller>() {
        Some(c) => Json(c.principal.whoami()).into_response(),
        None => auth_error(AuthError::Missing),
    }
}

async fn mirror(AxState(s): AxState<State>, req: Request) -> Response {
    let Some(caller) = req.extensions().get::<Caller>().cloned() else {
        return auth_error(AuthError::Missing);
    };
    let body = match axum::body::to_bytes(req.into_body(), 64 << 20).await {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("reading request: {e}")),
    };
    let r: MirrorRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("invalid mirror request: {e}")),
    };
    let mut seen = HashSet::new();
    let purls: Vec<String> = r.purls.into_iter().filter(|p| seen.insert(p.clone())).collect();
    if purls.is_empty() {
        return error(StatusCode::BAD_REQUEST, "no PURLs to mirror");
    }
    if purls.len() > s.max_purls {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "{} PURLs requested; at most {} are allowed",
                purls.len(),
                s.max_purls
            ),
        );
    }
    tracing::info!(purls = purls.len(), "mirror request");
    let stream = events(s, caller, purls);
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// The events of one job update, in order: logs, status, result.
fn update_events(purl: &str, u: Update) -> Vec<Event> {
    let mut out = Vec::new();
    for (level, message) in u.logs {
        out.push(Event::Log {
            purl: purl.to_string(),
            level,
            message,
        });
    }
    for st in u.statuses {
        out.push(Event::Status {
            purl: purl.to_string(),
            phase: st.phase,
            progress: st.progress,
        });
    }
    if let Some(outcome) = u.outcome {
        out.push(Event::Result {
            purl: purl.to_string(),
            outcome,
        });
    }
    out
}

fn fatal(purl: &str, message: String) -> Vec<Event> {
    vec![
        Event::Log {
            purl: purl.to_string(),
            level: LogLevel::Fatal,
            message,
        },
        Event::Result {
            purl: purl.to_string(),
            outcome: Outcome::Fatal,
        },
    ]
}

fn sse(e: &Event) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .event(e.name())
        .data(e.data())
}

/// Dispatch each PURL to its job and merge the jobs' updates into one
/// stream, which ends once every PURL has its result. Dropping the stream
/// (client gone) stops the forwarding, not the jobs.
fn events(
    s: State,
    caller: Caller,
    purls: Vec<String>,
) -> impl Stream<Item = Result<axum::response::sse::Event, Infallible>> {
    let (tx, rx) = mpsc::channel::<Vec<Event>>(1024);
    let phases = Event::Phases {
        phases: s.mirror.phases(),
    };
    let span = tracing::Span::current();
    tokio::spawn(
        async move {
            // `purl` is replaced for each PURL.
            let mut vars = caller.vars;
            for raw in purls {
                let purl = match Purl::parse(&raw) {
                    Ok(p) => p,
                    Err(e) => {
                        if tx.send(fatal(&raw, e.to_string())).await.is_err() {
                            return;
                        }
                        continue;
                    }
                };
                let mut pj = purl.to_json();
                pj["raw"] = raw.clone().into();
                vars.insert("purl".into(), pj);
                if let Err(e) = s.auth.purl.authorize(&vars) {
                    tracing::info!(purl = %raw, error = %e, "PURL not authorised");
                    if tx
                        .send(fatal(&raw, format!("not authorised to mirror this: {e}")))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                let job = match s.mirror.mirror(&purl) {
                    Ok(j) => j,
                    Err(e) => {
                        if tx.send(fatal(&raw, format!("{e:#}"))).await.is_err() {
                            return;
                        }
                        continue;
                    }
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut sub = job.subscribe();
                    while let Some(u) = sub.next().await {
                        let last = u.outcome.is_some();
                        if tx.send(update_events(&raw, u)).await.is_err() || last {
                            return;
                        }
                        tokio::time::sleep(UPDATE_INTERVAL).await;
                    }
                });
            }
        }
        .instrument(span),
    );
    let rest = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|e| (e, rx)) });
    futures::stream::once(async move { vec![phases] })
        .chain(rest)
        .flat_map(|evs| futures::stream::iter(evs.into_iter().map(|e| Ok(sse(&e)))))
}

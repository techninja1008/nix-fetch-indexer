//! HTTP routes: auth discovery, whoami and the mirror stream. Mirror
//! requests are authenticated and logged by `nfi_auth_server::middleware`.

use crate::job::Update;
use crate::{Auth, Mirror};
use axum::extract::{Request, State as AxState};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{Stream, StreamExt};
use nfi_auth_server::{AuthError, Caller, Vars};
use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::Instrument;
use yoink_api::{ErrorBody, Event, LogLevel, MIRROR_PATH, MirrorRequest, Outcome};
use yoink_purl::Purl;

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

/// A JSON error response for a bad mirror request.
fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ErrorBody { error: msg.into() })).into_response()
}

pub fn router(state: State) -> Router {
    let guard = state.auth.guard.clone();
    Router::new()
        .route(MIRROR_PATH, post(mirror))
        .layer(middleware::from_fn_with_state(
            guard.clone(),
            nfi_auth_server::middleware,
        ))
        .route("/healthz", get(|| async { "ok" }))
        .merge(guard.routes())
        .with_state(state)
}

async fn mirror(AxState(s): AxState<State>, req: Request) -> Response {
    let Some(Caller::User { vars, .. }) = req.extensions().get::<Caller>().cloned() else {
        return AuthError::Missing.into_response();
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
    let stream = events(s, vars, purls);
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

/// The SSE `data:` payload of `e` (the fields without the tag).
pub fn event_data(e: &Event) -> String {
    let mut v = serde_json::to_value(e).expect("serialisable");
    if let Some(o) = v.as_object_mut() {
        o.remove("event");
    }
    v.to_string()
}

fn sse(e: &Event) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .event(e.name())
        .data(event_data(e))
}

/// `p` for CEL policies: `{type, namespace, name, version, qualifiers, subpath}`.
pub fn purl_json(p: &Purl) -> serde_json::Value {
    serde_json::json!({
        "type": p.ty,
        "namespace": p.namespace,
        "name": p.name,
        "version": p.version,
        "qualifiers": p.qualifiers,
        "subpath": p.subpath,
    })
}

/// Dispatch each PURL to its job and merge the jobs' updates into one
/// stream, which ends once every PURL has its result. Dropping the stream
/// (client gone) stops the forwarding, not the jobs.
fn events(
    s: State,
    mut vars: Vars,
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
                let mut pj = purl_json(&purl);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purl_vars() {
        let p = Purl::parse("pkg:maven/org.apache%2Fx/commons-io@2.6?repo=central&a=b#src/main").unwrap();
        assert_eq!(purl_json(&p)["type"], "maven");
    }
}

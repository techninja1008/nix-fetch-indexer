//! HTTP routes for the public and admin listeners.

use crate::query::{self, LookupError};
use crate::{AppState, jobs};
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use nfi_api::drv::ReducedGraph;
use nfi_api::*;
use serde::Deserialize;

pub struct AppError(StatusCode, String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        let e = e.into();
        tracing::error!(error = %format!("{e:#}"), "request failed");
        AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

fn err(status: StatusCode, msg: impl Into<String>) -> AppError {
    AppError(status, msg.into())
}

type R<T> = Result<Json<T>, AppError>;

pub fn public_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/search", get(search))
        .route("/v1/packages/{*attr}", get(package))
        .route("/v1/lookup/bulk", post(bulk))
        .route("/v1/minimise", post(minimise))
        .layer(DefaultBodyLimit::max(512 << 20))
        .with_state(state)
}

pub fn admin_router(state: AppState) -> Router {
    Router::new()
        .route("/admin/v1/ingest", post(ingest))
        .route("/admin/v1/jobs", get(list_jobs))
        .route("/admin/v1/jobs/{id}", get(get_job))
        .route("/admin/v1/releases", get(releases))
        .route("/admin/v1/prune", post(prune))
        .route("/admin/v1/reindex", post(reindex))
        .route("/admin/v1/check", post(check))
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn_with_state(state.clone(), admin_auth))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state)
}

async fn admin_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if let Some(token) = &state.admin_token {
        let ok = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| constant_time_eq(t.as_bytes(), token.as_bytes()));
        if !ok {
            return err(StatusCode::UNAUTHORIZED, "missing or invalid admin token").into_response();
        }
    }
    next.run(req).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn search(State(s): State<AppState>, Query(q): Query<SearchQuery>) -> R<SearchResponse> {
    let Some(m) = &s.meili else {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "search is not configured"));
    };
    Ok(Json(m.search(&q).await?))
}

#[derive(Deserialize)]
struct PackageParams {
    commit: String,
    system: Option<String>,
}

async fn package(
    State(s): State<AppState>,
    Path(attr): Path<String>,
    Query(p): Query<PackageParams>,
) -> R<PackageInfo> {
    let system = p.system.unwrap_or_else(|| DEFAULT_SYSTEM.to_string());
    match query::lookup(&s.pool, &attr, &p.commit, &system).await? {
        Ok(info) => Ok(Json(info)),
        Err(LookupError::UnknownCommit) => {
            Err(err(StatusCode::NOT_FOUND, format!("unknown commit {}", p.commit)))
        }
        Err(LookupError::AmbiguousCommit) => Err(err(
            StatusCode::BAD_REQUEST,
            format!("ambiguous commit prefix {}", p.commit),
        )),
        Err(LookupError::NotFound) => Err(err(
            StatusCode::NOT_FOUND,
            format!("{attr} ({system}) not found at {}", p.commit),
        )),
    }
}

async fn bulk(State(s): State<AppState>, Json(req): Json<BulkRequest>) -> R<BulkResponse> {
    Ok(Json(query::bulk(&s.pool, &req).await?))
}

async fn minimise(State(s): State<AppState>, Json(v): Json<serde_json::Value>) -> R<MinimiseResponse> {
    let graph = ReducedGraph::from_json(&v).map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if graph.derivations.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "no derivations in request"));
    }
    Ok(Json(query::minimise(&s.pool, &graph).await?))
}

async fn ingest(State(s): State<AppState>, headers: HeaderMap, body: Body) -> Result<Response, AppError> {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    let id = if is_json {
        let bytes = axum::body::to_bytes(body, 1 << 20)
            .await
            .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
        let req: IngestUrlRequest = serde_json::from_slice(&bytes)
            .map_err(|e| err(StatusCode::BAD_REQUEST, format!("expected {{\"url\": ...}}: {e}")))?;
        jobs::enqueue(
            &s.pool,
            "ingest",
            serde_json::json!({"source": "url", "url": req.url}),
        )
        .await?
    } else {
        jobs::enqueue_upload(&s.pool, body.into_data_stream()).await?
    };
    Ok((StatusCode::ACCEPTED, Json(JobCreated { id })).into_response())
}

#[derive(Deserialize)]
struct ListParams {
    limit: Option<i64>,
}

async fn list_jobs(State(s): State<AppState>, Query(p): Query<ListParams>) -> R<Vec<JobInfo>> {
    Ok(Json(
        jobs::list(&s.pool, p.limit.unwrap_or(50).clamp(1, 1000)).await?,
    ))
}

async fn get_job(State(s): State<AppState>, Path(id): Path<i64>) -> R<JobInfo> {
    jobs::get(&s.pool, id)
        .await?
        .map(Json)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("no job {id}")))
}

async fn releases(State(s): State<AppState>) -> R<Vec<ReleaseInfo>> {
    let rows: Vec<(String, String, String, String, Option<String>, Vec<String>, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as(
            "SELECT r.name, r.commit, r.channel, r.series, r.url,
                    COALESCE(array_agg(d.dataset ORDER BY d.dataset) FILTER (WHERE d.dataset IS NOT NULL), '{}'),
                    r.indexed_at
             FROM releases r LEFT JOIN release_datasets d ON d.release_id = r.id
             GROUP BY r.id ORDER BY r.channel, r.indexed_at, r.name",
        )
        .fetch_all(&s.pool)
        .await?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(name, commit, channel, series, url, datasets, indexed_at)| ReleaseInfo {
                    name,
                    commit,
                    channel,
                    series,
                    url,
                    datasets,
                    indexed_at,
                },
            )
            .collect(),
    ))
}

async fn prune(State(s): State<AppState>, Json(req): Json<PruneRequest>) -> Result<Response, AppError> {
    if req.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "specify releases, commits or channels to prune",
        ));
    }
    let id = jobs::enqueue(&s.pool, "prune", serde_json::to_value(&req)?).await?;
    Ok((StatusCode::ACCEPTED, Json(JobCreated { id })).into_response())
}

async fn reindex(State(s): State<AppState>) -> Result<Response, AppError> {
    if s.meili.is_none() {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "search is not configured"));
    }
    let id = jobs::enqueue(&s.pool, "reindex", serde_json::json!({})).await?;
    Ok((StatusCode::ACCEPTED, Json(JobCreated { id })).into_response())
}

async fn check(State(s): State<AppState>) -> Result<Response, AppError> {
    let id = jobs::enqueue(&s.pool, "check", serde_json::json!({})).await?;
    Ok((StatusCode::ACCEPTED, Json(JobCreated { id })).into_response())
}

pub type RouterAlias = Router;

/// Serve a router on a listener until the process exits.
pub async fn serve(listener: tokio::net::TcpListener, router: Router) {
    if let Err(e) = axum::serve(listener, router).await {
        tracing::error!(error = %e, "server exited");
    }
}

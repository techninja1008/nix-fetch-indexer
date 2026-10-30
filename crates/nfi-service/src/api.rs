//! HTTP routes for the public and admin listeners.

use crate::auth::{self, AdminAuth, Caller};
use crate::query::{self, LookupError};
use crate::{AppState, jobs};
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware;
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
        .route("/v1/releases", get(releases))
        .route(nfi_auth::CONFIG_PATH, get(public_auth_config))
        .route(nfi_auth::WHOAMI_PATH, get(public_whoami))
        .layer(DefaultBodyLimit::max(512 << 20))
        .layer(middleware::from_fn_with_state(
            state.auth.clone(),
            auth::public_auth,
        ))
        .with_state(state)
}

pub fn admin_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/admin/v1/ingest", post(ingest))
        .route("/admin/v1/jobs", get(list_jobs))
        .route("/admin/v1/jobs/{id}", get(get_job))
        .route("/admin/v1/releases", get(releases))
        .route("/admin/v1/prune", post(prune))
        .route("/admin/v1/reindex", post(reindex))
        .route("/admin/v1/check", post(check))
        .route(nfi_auth::CONFIG_PATH, get(admin_auth_config))
        .route(nfi_auth::WHOAMI_PATH, get(admin_whoami))
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn_with_state(
            state.auth.clone(),
            auth::admin_auth,
        ))
        .with_state(state)
}

fn admin_oidc(s: &AppState) -> Option<&nfi_auth::server::Authenticator> {
    match s.auth.admin {
        AdminAuth::Oidc(_) => s.auth.oidc.as_ref(),
        _ => None,
    }
}

async fn public_auth_config(State(s): State<AppState>) -> Response {
    auth::config_response(s.auth.oidc.as_ref())
}

async fn admin_auth_config(State(s): State<AppState>) -> Response {
    auth::config_response(admin_oidc(&s))
}

async fn public_whoami(State(s): State<AppState>, caller: Option<axum::Extension<Caller>>) -> Response {
    match s.auth.oidc {
        Some(_) => auth::whoami_response(caller.as_ref().map(|c| &c.0)),
        None => auth::config_response(None),
    }
}

async fn admin_whoami(State(s): State<AppState>, caller: Option<axum::Extension<Caller>>) -> Response {
    match admin_oidc(&s) {
        Some(_) => auth::whoami_response(caller.as_ref().map(|c| &c.0)),
        None => auth::config_response(None),
    }
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

#[derive(Deserialize)]
struct MinimiseParams {
    /// See `nfi_api::minimise::close_miss_warnings`; 0 disables the check.
    close_miss_threshold: Option<u64>,
}

async fn minimise(
    State(s): State<AppState>,
    Query(p): Query<MinimiseParams>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> R<MinimiseResponse> {
    // Read from the bytes rather than through `Json`: a reduced graph is
    // deserialised directly, which is several times faster than via a `Value`.
    if !is_json(&headers) {
        return Err(err(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Expected request with `Content-Type: application/json`",
        ));
    }
    let graph =
        ReducedGraph::from_slice(&body).map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if graph.derivations.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "no derivations in request"));
    }
    let threshold = p
        .close_miss_threshold
        .unwrap_or(nfi_api::minimise::DEFAULT_CLOSE_MISS_THRESHOLD);
    Ok(Json(query::minimise(&s.pool, &graph, threshold).await?))
}

/// `application/json` or a `+json` type, as `Json` accepts.
fn is_json(headers: &HeaderMap) -> bool {
    let Some(ct) = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let essence = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    essence == "application/json" || (essence.starts_with("application/") && essence.ends_with("+json"))
}

#[derive(Deserialize)]
struct IngestParams {
    v2_closure_type: Option<String>,
}

async fn ingest(
    State(s): State<AppState>,
    Query(p): Query<IngestParams>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, AppError> {
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
        let v2 = req.v2_closure_type.or(p.v2_closure_type);
        jobs::enqueue(
            &s.pool,
            "ingest",
            serde_json::json!({"source": "url", "url": req.url, "v2_closure_type": v2}),
        )
        .await?
    } else {
        let params = serde_json::json!({"v2_closure_type": p.v2_closure_type});
        jobs::enqueue_upload(&s.pool, body.into_data_stream(), params).await?
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

#[derive(Deserialize)]
struct ReleasesParams {
    channel: Option<String>,
    /// List each evaluation's errors, not only their count.
    #[serde(default)]
    errors: bool,
}

/// Served on both listeners: what is in the index is public.
async fn releases(State(s): State<AppState>, Query(p): Query<ReleasesParams>) -> R<Vec<ReleaseInfo>> {
    type Row = (
        i32,
        String,
        String,
        String,
        String,
        Option<String>,
        Vec<String>,
        chrono::DateTime<chrono::Utc>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT r.id, r.name, r.commit, r.channel, r.series, r.url,
                COALESCE(array_agg(d.dataset ORDER BY d.dataset) FILTER (WHERE d.dataset IS NOT NULL), '{}'),
                r.indexed_at
         FROM releases r LEFT JOIN release_datasets d ON d.release_id = r.id
         WHERE $1::text IS NULL OR r.channel = $1
         GROUP BY r.id ORDER BY r.channel, r.indexed_at, r.name",
    )
    .bind(&p.channel)
    .fetch_all(&s.pool)
    .await?;
    type EvalRow = (i32, String, Vec<String>, Option<i64>, Option<Vec<String>>);
    let evals: Vec<EvalRow> = sqlx::query_as(
        "SELECT e.release_id, t.name,
                ARRAY(SELECT ct.name FROM closures c JOIN closure_types ct ON ct.id = c.type_id
                       WHERE c.eval_id = e.id ORDER BY ct.id),
                CASE WHEN e.errors_indexed THEN
                  (SELECT count(*) FROM eval_errors x WHERE x.eval_id = e.id)
                END,
                CASE WHEN e.errors_indexed AND $2 THEN
                  ARRAY(SELECT x.attr || '@' || x.system FROM eval_errors x WHERE x.eval_id = e.id
                         ORDER BY x.attr, x.system)
                END
           FROM evals e JOIN eval_types t ON t.id = e.type_id
          WHERE $1::text IS NULL OR e.release_id IN (SELECT id FROM releases WHERE channel = $1)
          ORDER BY e.release_id, t.id",
    )
    .bind(&p.channel)
    .bind(p.errors)
    .fetch_all(&s.pool)
    .await?;
    let mut evals_by_release: std::collections::HashMap<i32, Vec<EvalInfo>> = Default::default();
    for (release, eval_type, closures, error_count, errors) in evals {
        evals_by_release.entry(release).or_default().push(EvalInfo {
            eval_type,
            closures,
            error_count: error_count.map(|n| n as u64),
            errors,
        });
    }
    Ok(Json(
        rows.into_iter()
            .map(
                |(id, name, commit, channel, series, url, datasets, indexed_at)| ReleaseInfo {
                    name,
                    commit,
                    channel,
                    series,
                    url,
                    datasets,
                    evals: evals_by_release.remove(&id).unwrap_or_default(),
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

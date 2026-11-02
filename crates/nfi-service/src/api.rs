//! HTTP routes for the public and admin listeners.

use crate::query::{self, LookupError};
use crate::{AppState, jobs};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use nfi_api::*;

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
    let guard = state.auth.public.clone();
    Router::new()
        .route(SEARCH_PATH, get(search))
        .route(&format!("{PACKAGES_PATH}/{{*attr}}"), get(package))
        .route(BULK_PATH, post(bulk))
        .route(MINIMISE_PATH, post(minimise))
        .route(RELEASES_PATH, get(releases))
        .layer(DefaultBodyLimit::max(512 << 20))
        .layer(middleware::from_fn_with_state(
            guard.clone(),
            nfi_auth_server::middleware,
        ))
        .route("/healthz", get(|| async { "ok" }))
        .merge(guard.routes())
        .with_state(state)
}

pub fn admin_router(state: AppState) -> Router {
    let guard = state.auth.admin.clone();
    Router::new()
        .route(ADMIN_INGEST_PATH, post(ingest))
        .route(ADMIN_JOBS_PATH, get(list_jobs))
        .route(&format!("{ADMIN_JOBS_PATH}/{{id}}"), get(get_job))
        .route(ADMIN_RELEASES_PATH, get(releases))
        .route(ADMIN_PRUNE_PATH, post(prune))
        .route(ADMIN_REINDEX_PATH, post(reindex))
        .route(ADMIN_CHECK_PATH, post(check))
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn_with_state(
            guard.clone(),
            nfi_auth_server::middleware,
        ))
        .route("/healthz", get(|| async { "ok" }))
        .merge(guard.routes())
        .with_state(state)
}

async fn search(State(s): State<AppState>, Query(q): Query<SearchQuery>) -> R<SearchResponse> {
    let Some(m) = &s.meili else {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "search is not configured"));
    };
    Ok(Json(m.search(&q).await?))
}

async fn package(
    State(s): State<AppState>,
    Path(attr): Path<String>,
    Query(p): Query<PackageParams>,
) -> R<PackageInfo> {
    let system = p.system.unwrap_or_else(|| DEFAULT_SYSTEM.to_string());
    match query::lookup(&s.pool, &attr, &p.commit, &system).await? {
        Ok(info) => Ok(Json(info)),
        Err(LookupError::MalformedCommit(e)) => Err(err(StatusCode::BAD_REQUEST, e)),
        Err(LookupError::UnknownCommit) => {
            Err(err(StatusCode::NOT_FOUND, format!("unknown commit {}", p.commit)))
        }
        Err(LookupError::NotFound) => Err(err(
            StatusCode::NOT_FOUND,
            format!("{attr} ({system}) not found at {}", p.commit),
        )),
    }
}

async fn bulk(State(s): State<AppState>, Json(req): Json<BulkRequest>) -> R<BulkResponse> {
    match query::bulk(&s.pool, &req).await? {
        Ok(r) => Ok(Json(r)),
        Err(e) => Err(err(StatusCode::BAD_REQUEST, e)),
    }
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
    let graph = nfi_drv::parse(&body).map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if graph.derivations.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "no derivations in request"));
    }
    let threshold = p
        .close_miss_threshold
        .unwrap_or(crate::minimise::DEFAULT_CLOSE_MISS_THRESHOLD);
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
    // The job checks again, under the write lock.
    if let Some(e) = crate::prune::commit_error(&s.pool, &req).await? {
        return Err(err(StatusCode::BAD_REQUEST, e));
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

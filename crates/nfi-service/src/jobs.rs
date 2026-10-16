//! Postgres-backed job queue shared by all replicas.

use crate::AppState;
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use nfi_api::{JobInfo, JobStatus, PruneRequest};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::time::Duration;

pub const CHUNK: usize = 8 << 20;

#[derive(sqlx::FromRow)]
struct JobRow {
    id: i64,
    kind: String,
    status: String,
    params: Value,
    progress: Option<String>,
    result: Option<Value>,
    error: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn status(s: &str) -> JobStatus {
    match s {
        "running" => JobStatus::Running,
        "succeeded" => JobStatus::Succeeded,
        "failed" => JobStatus::Failed,
        _ => JobStatus::Queued,
    }
}

impl From<JobRow> for JobInfo {
    fn from(r: JobRow) -> Self {
        JobInfo {
            id: r.id,
            kind: r.kind,
            status: status(&r.status),
            params: r.params,
            progress: r.progress,
            result: r.result,
            error: r.error,
            created_at: r.created_at,
            started_at: r.started_at,
            finished_at: r.finished_at,
        }
    }
}

const COLS: &str = "id, kind, status, params, progress, result, error, created_at, started_at, finished_at";

pub async fn enqueue(pool: &PgPool, kind: &str, params: Value) -> Result<i64> {
    let id = sqlx::query_scalar("INSERT INTO jobs (kind, params) VALUES ($1, $2) RETURNING id")
        .bind(kind)
        .bind(params)
        .fetch_one(pool)
        .await?;
    sqlx::query("SELECT pg_notify('nfi_jobs', $1::text)")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(id)
}

/// Stage an uploaded bundle in chunks, then queue an ingest job for it.
pub async fn enqueue_upload<S, E>(pool: &PgPool, mut body: S) -> Result<i64>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::error::Error + Send + Sync + 'static,
{
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO jobs (kind, status, params) VALUES ('ingest', 'staging', '{}') RETURNING id",
    )
    .fetch_one(pool)
    .await?;
    let res: Result<usize> = async {
        let mut buf = Vec::with_capacity(CHUNK);
        let (mut seq, mut total) = (0i32, 0usize);
        while let Some(chunk) = body.next().await {
            let chunk = chunk.context("reading upload")?;
            total += chunk.len();
            buf.extend_from_slice(&chunk);
            if buf.len() >= CHUNK {
                store_chunk(pool, id, seq, &buf).await?;
                seq += 1;
                buf.clear();
            }
        }
        if !buf.is_empty() {
            store_chunk(pool, id, seq, &buf).await?;
        }
        Ok(total)
    }
    .await;
    match res {
        Ok(total) => {
            sqlx::query("UPDATE jobs SET status = 'queued', params = $2 WHERE id = $1")
                .bind(id)
                .bind(json!({"source": "upload", "bytes": total}))
                .execute(pool)
                .await?;
            sqlx::query("SELECT pg_notify('nfi_jobs', $1::text)")
                .bind(id)
                .execute(pool)
                .await?;
            Ok(id)
        }
        Err(e) => {
            sqlx::query("UPDATE jobs SET status = 'failed', error = $2, finished_at = now() WHERE id = $1")
                .bind(id)
                .bind(format!("{e:#}"))
                .execute(pool)
                .await?;
            sqlx::query("DELETE FROM job_blob_chunks WHERE job_id = $1")
                .bind(id)
                .execute(pool)
                .await?;
            Err(e)
        }
    }
}

async fn store_chunk(pool: &PgPool, job: i64, seq: i32, data: &[u8]) -> Result<()> {
    sqlx::query("INSERT INTO job_blob_chunks (job_id, seq, data) VALUES ($1, $2, $3)")
        .bind(job)
        .bind(seq)
        .bind(data)
        .execute(pool)
        .await?;
    Ok(())
}

async fn load_blob(pool: &PgPool, job: i64) -> Result<Vec<u8>> {
    let chunks: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT data FROM job_blob_chunks WHERE job_id = $1 ORDER BY seq")
            .bind(job)
            .fetch_all(pool)
            .await?;
    Ok(chunks.concat())
}

pub async fn get(pool: &PgPool, id: i64) -> Result<Option<JobInfo>> {
    Ok(
        sqlx::query_as::<_, JobRow>(&format!("SELECT {COLS} FROM jobs WHERE id = $1"))
            .bind(id)
            .fetch_optional(pool)
            .await?
            .map(Into::into),
    )
}

pub async fn list(pool: &PgPool, limit: i64) -> Result<Vec<JobInfo>> {
    Ok(
        sqlx::query_as::<_, JobRow>(&format!("SELECT {COLS} FROM jobs ORDER BY id DESC LIMIT $1"))
            .bind(limit)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    )
}

/// Claim the next runnable job: queued, or running with a stale heartbeat
/// (its replica died).
async fn claim(pool: &PgPool, worker: &str, stale: Duration) -> Result<Option<(i64, String, Value)>> {
    Ok(sqlx::query_as(
        "UPDATE jobs SET status = 'running', worker = $1, started_at = now(), heartbeat_at = now(), error = NULL
         WHERE id = (
             SELECT id FROM jobs
             WHERE status = 'queued'
                OR (status = 'running' AND heartbeat_at < now() - make_interval(secs => $2))
             ORDER BY id FOR UPDATE SKIP LOCKED LIMIT 1)
         RETURNING id, kind, params",
    )
    .bind(worker)
    .bind(stale.as_secs_f64())
    .fetch_optional(pool)
    .await?)
}

async fn finish(pool: &PgPool, id: i64, res: &Result<Value>) -> Result<()> {
    match res {
        Ok(v) => sqlx::query(
            "UPDATE jobs SET status = 'succeeded', result = $2, finished_at = now(), progress = NULL WHERE id = $1",
        )
        .bind(id)
        .bind(v)
        .execute(pool)
        .await?,
        Err(e) => sqlx::query("UPDATE jobs SET status = 'failed', error = $2, finished_at = now() WHERE id = $1")
            .bind(id)
            .bind(format!("{e:#}"))
            .execute(pool)
            .await?,
    };
    sqlx::query("DELETE FROM job_blob_chunks WHERE job_id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn run_job(state: &AppState, id: i64, kind: &str, params: &Value) -> Result<Value> {
    let pool = &state.pool;
    let progress_pool = pool.clone();
    let progress = move |msg: String| {
        tracing::info!(job = id, "{msg}");
        let p = progress_pool.clone();
        tokio::spawn(async move {
            sqlx::query("UPDATE jobs SET progress = $2 WHERE id = $1")
                .bind(id)
                .bind(msg)
                .execute(&p)
                .await
                .ok();
        });
    };
    match kind {
        "ingest" => {
            let data = if let Some(url) = params.get("url").and_then(Value::as_str) {
                progress(format!("downloading {url}"));
                let resp = state.http.get(url).send().await?.error_for_status()?;
                resp.bytes().await?.to_vec()
            } else {
                load_blob(pool, id).await?
            };
            progress(format!("decoding {} bytes", data.len()));
            let bundle =
                tokio::task::spawn_blocking(move || nfi_interchange::codec::read(&data[..])).await??;
            let report = crate::ingest::ingest(pool, &bundle, &progress).await?;
            let mut result = serde_json::to_value(&report)?;
            if let Some(m) = &state.meili {
                progress(format!(
                    "updating search for {} attributes",
                    report.touched_attrs.len()
                ));
                let n = m.sync_attrs(pool, &report.touched_attrs).await?;
                result["search_documents"] = json!(n);
            }
            Ok(result)
        }
        "prune" => {
            let req: PruneRequest = serde_json::from_value(params.clone())?;
            let report = crate::prune::prune(pool, &req).await?;
            let mut result = serde_json::to_value(&report)?;
            if let (Some(m), false) = (&state.meili, report.releases.is_empty()) {
                progress("rebuilding search index".into());
                result["search_documents"] = json!(m.reindex(pool).await?);
            }
            Ok(result)
        }
        "reindex" => {
            let Some(m) = &state.meili else {
                bail!("search is not configured")
            };
            Ok(json!({"search_documents": m.reindex(pool).await?}))
        }
        other => bail!("unknown job kind {other:?}"),
    }
}

/// Process jobs until the process exits.
pub async fn worker(state: AppState, worker_id: String) {
    let stale = Duration::from_secs(120);
    let mut listener = match sqlx::postgres::PgListener::connect_with(&state.pool).await {
        Ok(mut l) => match l.listen("nfi_jobs").await {
            Ok(()) => Some(l),
            Err(_) => None,
        },
        Err(_) => None,
    };
    loop {
        match claim(&state.pool, &worker_id, stale).await {
            Ok(Some((id, kind, params))) => {
                tracing::info!(job = id, %kind, "running job");
                let hb_pool = state.pool.clone();
                let heartbeat = tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(15)).await;
                        sqlx::query("UPDATE jobs SET heartbeat_at = now() WHERE id = $1")
                            .bind(id)
                            .execute(&hb_pool)
                            .await
                            .ok();
                    }
                });
                let res = run_job(&state, id, &kind, &params).await;
                heartbeat.abort();
                if let Err(e) = &res {
                    tracing::error!(job = id, error = %format!("{e:#}"), "job failed");
                }
                if let Err(e) = finish(&state.pool, id, &res).await {
                    tracing::error!(job = id, error = %e, "recording job result failed");
                }
                continue;
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "claiming job failed"),
        }
        // Wait for a notification or poll interval.
        match &mut listener {
            Some(l) => {
                let _ = tokio::time::timeout(state.poll_interval, l.recv()).await;
            }
            None => tokio::time::sleep(state.poll_interval).await,
        }
    }
}

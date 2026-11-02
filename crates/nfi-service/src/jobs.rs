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
pub async fn enqueue_upload<S, E>(pool: &PgPool, mut body: S, params: Value) -> Result<i64>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::error::Error + Send + Sync + 'static,
{
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO jobs (kind, status, params) VALUES ('ingest', 'staging', $1) RETURNING id",
    )
    .bind(params)
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
            sqlx::query("UPDATE jobs SET status = 'queued', params = params || $2 WHERE id = $1")
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

async fn load_blob(pool: &PgPool, job: i64) -> Result<Bytes> {
    let chunks: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT data FROM job_blob_chunks WHERE job_id = $1 ORDER BY seq")
            .bind(job)
            .fetch_all(pool)
            .await?;
    Ok(Bytes::from(chunks.concat()))
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

/// A job claimed by this worker.
pub struct Claim {
    pub id: i64,
    pub kind: String,
    pub params: Value,
    /// Fencing token of this claim (a uuid). Updates for the job only apply
    /// while the job still carries it, i.e. until another worker reclaims it.
    claim_id: String,
}

/// Claim the next runnable job: queued, or running with a stale heartbeat
/// (its replica died, or lost touch with the database).
pub async fn claim(pool: &PgPool, worker: &str, stale: Duration) -> Result<Option<Claim>> {
    let row: Option<(i64, String, Value, String)> = sqlx::query_as(
        "UPDATE jobs SET status = 'running', worker = $1, claim_id = gen_random_uuid(),
                started_at = now(), heartbeat_at = now(), error = NULL
         WHERE id = (
             SELECT id FROM jobs
             WHERE status = 'queued'
                OR (status = 'running' AND heartbeat_at < now() - make_interval(secs => $2))
             ORDER BY id FOR UPDATE SKIP LOCKED LIMIT 1)
         RETURNING id, kind, params, claim_id::text",
    )
    .bind(worker)
    .bind(stale.as_secs_f64())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, kind, params, claim_id)| Claim {
        id,
        kind,
        params,
        claim_id,
    }))
}

/// Bump the job's heartbeat and, when given, set its progress. Returns false
/// when the claim was lost.
pub async fn touch(pool: &PgPool, job: &Claim, progress: Option<&str>) -> Result<bool> {
    let r = sqlx::query(
        "UPDATE jobs SET heartbeat_at = now(), progress = COALESCE($3, progress)
         WHERE id = $1 AND claim_id = $2::uuid",
    )
    .bind(job.id)
    .bind(&job.claim_id)
    .bind(progress)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Record the job's result and drop its staged upload. Returns false (and
/// records nothing) when the claim was lost. A failed job keeps its last
/// progress (`progress`, when given, is a newer one to record).
pub async fn finish(pool: &PgPool, job: &Claim, res: &Result<Value>, progress: Option<&str>) -> Result<bool> {
    let r = match res {
        Ok(v) => {
            sqlx::query(
                "UPDATE jobs SET status = 'succeeded', result = $3, finished_at = now(), progress = NULL
                 WHERE id = $1 AND claim_id = $2::uuid",
            )
            .bind(job.id)
            .bind(&job.claim_id)
            .bind(v)
            .execute(pool)
            .await?
        }
        Err(e) => {
            sqlx::query(
                "UPDATE jobs SET status = 'failed', error = $3, finished_at = now(),
                        progress = COALESCE($4, progress)
                 WHERE id = $1 AND claim_id = $2::uuid",
            )
            .bind(job.id)
            .bind(&job.claim_id)
            .bind(format!("{e:#}"))
            .bind(progress)
            .execute(pool)
            .await?
        }
    };
    if r.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query("DELETE FROM job_blob_chunks WHERE job_id = $1")
        .bind(job.id)
        .execute(pool)
        .await?;
    Ok(true)
}

/// A running job's latest progress message, published by its keep-alive task.
pub struct Progress {
    job: i64,
    /// The latest message not yet written to the database.
    pending: std::sync::Mutex<Option<String>>,
}

impl Progress {
    fn new(job: i64) -> Self {
        Self {
            job,
            pending: Default::default(),
        }
    }

    pub fn set(&self, msg: String) {
        tracing::info!(job = self.job, "{msg}");
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
    }

    fn take(&self) -> Option<String> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// How often the keep-alive task writes changed progress.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
/// How often the keep-alive task bumps the heartbeat.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// Write progress when it changes and the heartbeat periodically, until
/// `stop` fires (returns false) or the claim is lost (returns true). Queries
/// are never interrupted, so once this returns nothing more is written.
async fn keep_alive(
    pool: PgPool,
    job: std::sync::Arc<Claim>,
    progress: std::sync::Arc<Progress>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) -> bool {
    let mut tick = tokio::time::interval(PROGRESS_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut beat = tokio::time::Instant::now();
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => return false,
            _ = tick.tick() => {}
        }
        let msg = progress.take();
        if msg.is_none() && beat.elapsed() < HEARTBEAT_INTERVAL {
            continue;
        }
        match touch(&pool, &job, msg.as_deref()).await {
            Ok(true) => beat = tokio::time::Instant::now(),
            Ok(false) => return true,
            Err(e) => {
                tracing::warn!(job = job.id, error = %e, "job heartbeat failed");
                // Retry the message on the next tick, unless a newer one came.
                if let Some(m) = msg {
                    progress
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_or_insert(m);
                }
            }
        }
    }
}

/// Run a claimed job: `run` executes while a keep-alive task publishes its
/// progress and heartbeat, then the result is recorded. If the claim is lost
/// meanwhile (another worker reclaimed the job), `run` is cancelled and
/// nothing is recorded; returns false then.
pub async fn execute<F, Fut>(pool: &PgPool, job: Claim, run: F) -> bool
where
    F: FnOnce(std::sync::Arc<Claim>, std::sync::Arc<Progress>) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    use std::sync::Arc;
    let job = Arc::new(job);
    let progress = Arc::new(Progress::new(job.id));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let mut keeper = tokio::spawn(keep_alive(pool.clone(), job.clone(), progress.clone(), stopped));
    let res = tokio::select! {
        r = run(job.clone(), progress.clone()) => Some(r),
        _ = &mut keeper => None,
    };
    // Stop the keep-alive task before recording the result, so no progress
    // write lands after it.
    let lost = match res {
        None => true,
        Some(_) => {
            let _ = stop.send(());
            keeper.await.unwrap_or(false)
        }
    };
    let res = match res {
        Some(r) if !lost => r,
        _ => {
            tracing::warn!(
                job = job.id,
                "job was reclaimed by another worker (heartbeat lost); abandoning it"
            );
            return false;
        }
    };
    if let Err(e) = &res {
        tracing::error!(job = job.id, error = %format!("{e:#}"), "job failed");
    }
    match finish(pool, &job, &res, progress.take().as_deref()).await {
        Ok(true) => true,
        Ok(false) => {
            tracing::warn!(
                job = job.id,
                "job was reclaimed by another worker before its result was recorded; discarding it"
            );
            false
        }
        Err(e) => {
            tracing::error!(job = job.id, error = %e, "recording job result failed");
            true
        }
    }
}

async fn run_job(state: &AppState, job: &Claim, progress: &Progress) -> Result<Value> {
    let pool = &state.pool;
    let (id, params) = (job.id, &job.params);
    let progress = |msg: String| progress.set(msg);
    match job.kind.as_str() {
        "ingest" => {
            let data = if let Some(url) = params.get("url").and_then(Value::as_str) {
                progress(format!("downloading {url}"));
                let resp = state.http.get(url).send().await?.error_for_status()?;
                resp.bytes().await?
            } else {
                load_blob(pool, id).await?
            };
            progress(format!("decoding {} bytes", data.len()));
            let v2 = params
                .get("v2_closure_type")
                .and_then(Value::as_str)
                .unwrap_or(nfi_interchange::STORE_PATHS)
                .to_string();
            let bundle =
                tokio::task::spawn_blocking(move || nfi_interchange::codec::read(&data[..], &v2)).await??;
            let (mut report, touched_attrs) = crate::ingest::ingest(pool, &bundle, &progress).await?;
            if let Some(m) = &state.meili {
                progress(format!("updating search for {} attributes", touched_attrs.len()));
                report.search_documents = Some(m.sync_attrs(pool, &touched_attrs).await?);
            }
            Ok(serde_json::to_value(&report)?)
        }
        "prune" => {
            let req: PruneRequest = serde_json::from_value(params.clone())?;
            let mut report = crate::prune::prune(pool, &req).await?;
            if let (Some(m), false) = (&state.meili, report.releases.is_empty()) {
                progress("rebuilding search index".into());
                report.search_documents = Some(m.reindex(pool).await?);
            }
            Ok(serde_json::to_value(&report)?)
        }
        "reindex" => {
            let Some(m) = &state.meili else {
                bail!("search is not configured")
            };
            Ok(json!({"search_documents": m.reindex(pool).await?}))
        }
        "check" => {
            progress("checking index consistency".into());
            let report = crate::check::check(pool).await?;
            Ok(serde_json::to_value(&report)?)
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
            Ok(Some(job)) => {
                tracing::info!(job = job.id, kind = %job.kind, "running job");
                let st = &state;
                execute(&state.pool, job, |job, progress| async move {
                    run_job(st, &job, &progress).await
                })
                .await;
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

//! HTTP client for the query service (public and admin APIs).

use crate::*;
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;

/// A non-success HTTP response from the service.
#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match reqwest::StatusCode::from_u16(self.status) {
            Ok(s) => write!(f, "HTTP {s}: {}", self.message),
            Err(_) => write!(f, "HTTP {}: {}", self.status, self.message),
        }
    }
}

impl std::error::Error for HttpError {}

impl HttpError {
    /// Whether `e` is (or wraps) a 401 response.
    pub fn is_unauthorized(e: &anyhow::Error) -> bool {
        e.downcast_ref::<HttpError>().is_some_and(|h| h.status == 401)
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl Client {
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Whether requests carry a bearer token.
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    pub fn new(base: &str, token: Option<String>) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(concat!("nfi/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(3600))
                .build()?,
            base: base.trim_end_matches('/').to_string(),
            token,
        })
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let r = self.http.request(method, format!("{}{path}", self.base));
        match &self.token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }

    async fn send<T: serde::de::DeserializeOwned>(&self, rb: reqwest::RequestBuilder) -> Result<T> {
        let resp = rb
            .send()
            .await
            .with_context(|| format!("connecting to {}", self.base))?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            let msg = serde_json::from_slice::<ErrorBody>(&bytes)
                .map(|e| e.error)
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
            return Err(HttpError {
                status: status.as_u16(),
                message: msg,
            }
            .into());
        }
        serde_json::from_slice(&bytes).context("decoding response")
    }

    /// GET a JSON document, e.g. `nfi_auth::WHOAMI_PATH`.
    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.send(self.req(reqwest::Method::GET, path)).await
    }

    // ----- public -----

    pub async fn search(&self, q: &SearchQuery) -> Result<SearchResponse> {
        self.send(self.req(reqwest::Method::GET, "/v1/search").query(q))
            .await
    }

    pub async fn package(&self, attr: &str, commit: &str, system: Option<&str>) -> Result<PackageInfo> {
        let mut q = vec![("commit", commit)];
        if let Some(s) = system {
            q.push(("system", s));
        }
        self.send(
            self.req(reqwest::Method::GET, &format!("/v1/packages/{attr}"))
                .query(&q),
        )
        .await
    }

    pub async fn bulk(&self, req: &BulkRequest) -> Result<BulkResponse> {
        self.send(self.req(reqwest::Method::POST, "/v1/lookup/bulk").json(req))
            .await
    }

    /// `close_miss_threshold`: see `minimise::close_miss_warnings` (server
    /// default when `None`, 0 disables).
    pub async fn minimise(
        &self,
        graph: &drv::ReducedGraph,
        close_miss_threshold: Option<u64>,
    ) -> Result<MinimiseResponse> {
        let mut rb = self.req(reqwest::Method::POST, "/v1/minimise");
        if let Some(t) = close_miss_threshold {
            rb = rb.query(&[("close_miss_threshold", t)]);
        }
        self.send(rb.json(graph)).await
    }

    /// Indexed releases, optionally only those in `channel`; `errors` also
    /// lists each evaluation's errors, not only their count.
    pub async fn releases(&self, channel: Option<&str>, errors: bool) -> Result<Vec<ReleaseInfo>> {
        self.releases_at("/v1/releases", channel, errors).await
    }

    async fn releases_at(&self, path: &str, channel: Option<&str>, errors: bool) -> Result<Vec<ReleaseInfo>> {
        let mut rb = self.req(reqwest::Method::GET, path);
        if let Some(c) = channel {
            rb = rb.query(&[("channel", c)]);
        }
        if errors {
            rb = rb.query(&[("errors", "true")]);
        }
        self.send(rb).await
    }

    // ----- admin -----

    /// [`Client::releases`] through the admin listener.
    pub async fn admin_releases(&self, channel: Option<&str>, errors: bool) -> Result<Vec<ReleaseInfo>> {
        self.releases_at("/admin/v1/releases", channel, errors).await
    }

    /// Upload a bundle; `v2_closure_type` is the closure type to read a
    /// version 2 bundle as (server default `store-paths`).
    pub async fn ingest_file(&self, path: &Path, v2_closure_type: Option<&str>) -> Result<JobCreated> {
        let f = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        let len = f.metadata().await?.len();
        let stream = tokio_util::io::ReaderStream::with_capacity(f, 1 << 20);
        let body = reqwest::Body::wrap_stream(stream);
        let mut rb = self.req(reqwest::Method::POST, "/admin/v1/ingest");
        if let Some(t) = v2_closure_type {
            rb = rb.query(&[("v2_closure_type", t)]);
        }
        self.send(
            rb.header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .header(reqwest::header::CONTENT_LENGTH, len)
                .body(body),
        )
        .await
    }

    pub async fn ingest_url(&self, url: &str, v2_closure_type: Option<&str>) -> Result<JobCreated> {
        self.send(
            self.req(reqwest::Method::POST, "/admin/v1/ingest")
                .json(&IngestUrlRequest {
                    url: url.into(),
                    v2_closure_type: v2_closure_type.map(str::to_string),
                }),
        )
        .await
    }

    pub async fn job(&self, id: i64) -> Result<JobInfo> {
        self.send(self.req(reqwest::Method::GET, &format!("/admin/v1/jobs/{id}")))
            .await
    }

    pub async fn jobs(&self, limit: usize) -> Result<Vec<JobInfo>> {
        self.send(
            self.req(reqwest::Method::GET, "/admin/v1/jobs")
                .query(&[("limit", limit)]),
        )
        .await
    }

    pub async fn prune(&self, req: &PruneRequest) -> Result<JobCreated> {
        self.send(self.req(reqwest::Method::POST, "/admin/v1/prune").json(req))
            .await
    }

    pub async fn reindex(&self) -> Result<JobCreated> {
        self.send(self.req(reqwest::Method::POST, "/admin/v1/reindex"))
            .await
    }

    /// Queue a consistency check of the closure index.
    pub async fn check(&self) -> Result<JobCreated> {
        self.send(self.req(reqwest::Method::POST, "/admin/v1/check"))
            .await
    }

    /// Poll a job until it finishes, calling `on_progress` when its progress changes.
    pub async fn wait_job(&self, id: i64, mut on_progress: impl FnMut(&JobInfo)) -> Result<JobInfo> {
        let mut last: Option<String> = None;
        let mut delay = Duration::from_millis(100);
        loop {
            let j = self.job(id).await?;
            if j.progress != last {
                on_progress(&j);
                last = j.progress.clone();
            }
            if j.status.is_finished() {
                return Ok(j);
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
        }
    }
}

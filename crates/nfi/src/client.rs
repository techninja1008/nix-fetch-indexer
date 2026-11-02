//! HTTP client for the query service (public and admin APIs).

use crate::http::check;
use anyhow::{Context, Result};
use nfi_api::*;
use std::path::Path;
use std::time::Duration;

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

    /// The same client (sharing its connections) sending `token`.
    pub fn with_token(&self, token: Option<String>) -> Self {
        Self {
            token,
            ..self.clone()
        }
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
        let bytes = check(resp).await?.bytes().await?;
        serde_json::from_slice(&bytes).context("decoding response")
    }

    /// GET a JSON document, e.g. `nfi_auth::WHOAMI_PATH`.
    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.send(self.req(reqwest::Method::GET, path)).await
    }

    // ----- public -----

    pub async fn search(&self, q: &SearchQuery) -> Result<SearchResponse> {
        self.send(self.req(reqwest::Method::GET, SEARCH_PATH).query(q))
            .await
    }

    pub async fn package(&self, attr: &str, commit: &str, system: Option<&str>) -> Result<PackageInfo> {
        let q = PackageParams {
            commit: commit.into(),
            system: system.map(str::to_string),
        };
        self.send(
            self.req(reqwest::Method::GET, &format!("{PACKAGES_PATH}/{attr}"))
                .query(&q),
        )
        .await
    }

    pub async fn bulk(&self, req: &BulkRequest) -> Result<BulkResponse> {
        self.send(self.req(reqwest::Method::POST, BULK_PATH).json(req))
            .await
    }

    /// `close_miss_threshold`: see [`MinimiseParams`].
    pub async fn minimise(
        &self,
        graph: &drv::ReducedGraph,
        close_miss_threshold: Option<u64>,
    ) -> Result<MinimiseResponse> {
        let q = MinimiseParams { close_miss_threshold };
        self.send(
            self.req(reqwest::Method::POST, MINIMISE_PATH)
                .query(&q)
                .json(graph),
        )
        .await
    }

    /// Indexed releases, optionally only those in `channel`; `errors` also
    /// lists each evaluation's errors, not only their count.
    pub async fn releases(&self, channel: Option<&str>, errors: bool) -> Result<Vec<ReleaseInfo>> {
        self.releases_at(RELEASES_PATH, channel, errors).await
    }

    async fn releases_at(&self, path: &str, channel: Option<&str>, errors: bool) -> Result<Vec<ReleaseInfo>> {
        let q = ReleasesParams {
            channel: channel.map(str::to_string),
            errors,
        };
        self.send(self.req(reqwest::Method::GET, path).query(&q)).await
    }

    // ----- admin -----

    /// [`Client::releases`] through the admin listener.
    pub async fn admin_releases(&self, channel: Option<&str>, errors: bool) -> Result<Vec<ReleaseInfo>> {
        self.releases_at(ADMIN_RELEASES_PATH, channel, errors).await
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
        let q = IngestParams {
            v2_closure_type: v2_closure_type.map(str::to_string),
        };
        self.send(
            self.req(reqwest::Method::POST, ADMIN_INGEST_PATH)
                .query(&q)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .header(reqwest::header::CONTENT_LENGTH, len)
                .body(body),
        )
        .await
    }

    pub async fn ingest_url(&self, url: &str, v2_closure_type: Option<&str>) -> Result<JobCreated> {
        self.send(
            self.req(reqwest::Method::POST, ADMIN_INGEST_PATH)
                .json(&IngestUrlRequest {
                    url: url.into(),
                    v2_closure_type: v2_closure_type.map(str::to_string),
                }),
        )
        .await
    }

    pub async fn job(&self, id: i64) -> Result<JobInfo> {
        self.send(self.req(reqwest::Method::GET, &format!("{ADMIN_JOBS_PATH}/{id}")))
            .await
    }

    pub async fn jobs(&self, limit: usize) -> Result<Vec<JobInfo>> {
        let q = ListParams {
            limit: Some(limit.try_into().unwrap_or(i64::MAX)),
        };
        self.send(self.req(reqwest::Method::GET, ADMIN_JOBS_PATH).query(&q))
            .await
    }

    pub async fn prune(&self, req: &PruneRequest) -> Result<JobCreated> {
        self.send(self.req(reqwest::Method::POST, ADMIN_PRUNE_PATH).json(req))
            .await
    }

    pub async fn reindex(&self) -> Result<JobCreated> {
        self.send(self.req(reqwest::Method::POST, ADMIN_REINDEX_PATH))
            .await
    }

    /// Queue a consistency check of the closure index.
    pub async fn check(&self) -> Result<JobCreated> {
        self.send(self.req(reqwest::Method::POST, ADMIN_CHECK_PATH)).await
    }
}

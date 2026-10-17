//! HTTP fetching with retries.

use anyhow::{Context, Result, bail};
use std::time::Duration;

#[derive(Clone)]
pub struct Fetcher {
    /// Independent clients, each with its own connection pool. HTTP/2
    /// servers cap concurrent streams per connection (Fastly: ~100), so
    /// spreading requests over several connections raises throughput.
    clients: std::sync::Arc<Vec<reqwest::Client>>,
    next: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub retries: u32,
}

/// Outcome of fetching a resource that may legitimately be absent.
pub enum Fetched {
    Found(bytes::Bytes),
    NotFound,
}

impl Fetcher {
    pub fn new(retries: u32) -> Result<Self> {
        Self::with_connections(retries, 1)
    }

    pub fn with_connections(retries: u32, connections: usize) -> Result<Self> {
        let clients = (0..connections.max(1))
            .map(|_| {
                reqwest::Client::builder()
                    .user_agent(concat!("nix-fetch-indexer/", env!("CARGO_PKG_VERSION")))
                    .connect_timeout(Duration::from_secs(30))
                    .timeout(Duration::from_secs(600))
                    .pool_max_idle_per_host(256)
                    .build()
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            clients: std::sync::Arc::new(clients),
            next: Default::default(),
            retries,
        })
    }

    pub fn client(&self) -> &reqwest::Client {
        let i = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        &self.clients[i % self.clients.len()]
    }

    /// Establish one connection per client (sequentially per client, in
    /// parallel across clients) so a burst of requests reuses them instead
    /// of each opening its own connection.
    pub async fn warm_up(&self, url: &str) {
        let futs = self.clients.iter().map(|c| async move {
            let _ = c.get(url).send().await.map(|r| r.bytes());
        });
        futures::future::join_all(futs).await;
    }

    /// GET a URL, returning `NotFound` on 404 (and 403, which S3 returns for
    /// missing keys without list permission). Retries transient failures with
    /// exponential backoff.
    pub async fn get_opt(&self, url: &str) -> Result<Fetched> {
        let mut attempt = 0;
        loop {
            let err = match self.client().get(url).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        match resp.bytes().await {
                            Ok(b) => return Ok(Fetched::Found(b)),
                            Err(e) => anyhow::Error::from(e),
                        }
                    } else if status == reqwest::StatusCode::NOT_FOUND
                        || status == reqwest::StatusCode::FORBIDDEN
                    {
                        return Ok(Fetched::NotFound);
                    } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                        anyhow::anyhow!("HTTP {status}")
                    } else {
                        bail!("GET {url}: HTTP {status}");
                    }
                }
                Err(e) => e.into(),
            };
            attempt += 1;
            if attempt > self.retries {
                return Err(err).with_context(|| format!("GET {url} failed after {attempt} attempts"));
            }
            let delay = Duration::from_millis(250 * 2u64.pow(attempt.min(8)));
            tracing::debug!(%url, attempt, ?delay, error = %err, "retrying");
            tokio::time::sleep(delay).await;
        }
    }

    pub async fn get(&self, url: &str) -> Result<bytes::Bytes> {
        match self.get_opt(url).await? {
            Fetched::Found(b) => Ok(b),
            Fetched::NotFound => bail!("GET {url}: not found"),
        }
    }

    pub async fn get_text(&self, url: &str) -> Result<String> {
        let b = self.get(url).await?;
        String::from_utf8(b.to_vec()).with_context(|| format!("{url}: not UTF-8"))
    }
}

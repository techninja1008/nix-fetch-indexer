//! A yoink [`Mirror`] that copies store paths from one HTTP binary cache
//! (cache.nixos.org by default) to another that accepts `PUT`s, such as a
//! plain WebDAV-style server or a GitLab generic package registry.
//!
//! A job per store path hash:
//!
//! 1. **Queued** until one of `--concurrency` slots is free.
//! 2. **Checking target**: `GET <target>/<hash>.narinfo`; present means done.
//! 3. **Fetching narinfo** from the upstream cache.
//! 4. **Transferring NAR**: the upstream NAR is streamed into a `PUT` to the
//!    target, checking its size and `FileHash` on the way.
//! 5. **Publishing narinfo**: the narinfo is uploaded last, with its `URL`
//!    pointed at the copied NAR and its signatures kept (plus ours with
//!    `--signing-key-file`), so a narinfo on the target means a complete path.
//!
//! Transient failures (connection errors, timeouts, HTTP 408/429/5xx, a
//! corrupted transfer) are retried with backoff, `--retries` times per step.

pub mod narinfo;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use narinfo::{Narinfo, Signer};
use nfi_interchange::{StoreHash, nixbase32};
use reqwest::header::{HeaderName, HeaderValue};
use sha2::Digest;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use yoink_api::{NixTarget, PhaseDef, Purl, Unit};
use yoink_server::{Job, JobReporter, JobTable, Mirror};

pub const QUEUED: &str = "queued";
pub const CHECKING: &str = "checking";
pub const NARINFO: &str = "narinfo";
pub const TRANSFER: &str = "transfer";
pub const PUBLISH: &str = "publish";

/// How NAR files are named on the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum NarLayout {
    /// `nar/<file>`, like cache.nixos.org.
    Nar,
    /// `<file>` next to the narinfos, for stores that don't allow `/` in
    /// file names (e.g. GitLab generic packages).
    Flat,
}

/// How to authenticate to a cache.
#[derive(Clone, Debug, Default)]
pub enum Credential {
    #[default]
    None,
    Basic {
        user: String,
        password: String,
    },
    Bearer(String),
    /// An arbitrary header, e.g. GitLab's `PRIVATE-TOKEN` or `JOB-TOKEN`.
    Header(HeaderName, HeaderValue),
}

impl Credential {
    fn apply(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Credential::None => rb,
            Credential::Basic { user, password } => rb.basic_auth(user, Some(password)),
            Credential::Bearer(t) => rb.bearer_auth(t),
            Credential::Header(n, v) => rb.header(n, v),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Endpoint {
    /// Base URL, without a trailing slash.
    pub url: String,
    pub credential: Credential,
}

impl Endpoint {
    pub fn new(url: &str, credential: Credential) -> Self {
        Endpoint {
            url: url.trim_end_matches('/').to_string(),
            credential,
        }
    }
}

pub struct Config {
    pub upstream: Endpoint,
    pub target: Endpoint,
    pub layout: NarLayout,
    pub signer: Option<Signer>,
    /// Jobs transferring at once.
    pub concurrency: usize,
    /// Retries per step for transient failures.
    pub retries: u32,
    /// First retry delay; doubled each time, up to 30 s.
    pub retry_delay: Duration,
    /// Upload `nix-cache-info` with this priority if the target has none.
    pub cache_info_priority: Option<u32>,
    pub timeout: Duration,
}

/// Credential options for one cache. At most one may be given.
macro_rules! credential_args {
    ($name:ident, $side:literal, $basic:literal, $bearer:literal, $hname:literal, $hfile:literal) => {
        #[derive(clap::Args, Debug, Clone)]
        pub struct $name {
            #[arg(help = concat!("File holding `user:password` for basic auth to the ", $side, " cache"),
                  id = concat!($side, "_basic_auth_file"), long = concat!($side, "-basic-auth-file"), env = $basic,
                  value_name = "FILE")]
            pub basic_auth_file: Option<PathBuf>,
            #[arg(help = concat!("File holding a bearer token for the ", $side, " cache"),
                  id = concat!($side, "_bearer_token_file"), long = concat!($side, "-bearer-token-file"),
                  env = $bearer, value_name = "FILE", conflicts_with = concat!($side, "_basic_auth_file"))]
            pub bearer_token_file: Option<PathBuf>,
            #[arg(help = concat!("Header to send to the ", $side, " cache (e.g. PRIVATE-TOKEN), with its value from --",
                            $side, "-header-file"),
                  id = concat!($side, "_header_name"), value_name = "NAME", long = concat!($side, "-header-name"), env = $hname,
                  requires = concat!($side, "_header_file"),
                  conflicts_with_all = [concat!($side, "_basic_auth_file"), concat!($side, "_bearer_token_file")])]
            pub header_name: Option<String>,
            #[arg(help = concat!("File holding the value of --", $side, "-header-name"),
                  id = concat!($side, "_header_file"), long = concat!($side, "-header-file"), env = $hfile,
                  value_name = "FILE", requires = concat!($side, "_header_name"))]
            pub header_file: Option<PathBuf>,
        }

        impl $name {
            pub fn credential(&self) -> Result<Credential> {
                let read = |p: &PathBuf| read_secret(p);
                if let Some(p) = &self.basic_auth_file {
                    let s = read(p)?;
                    let (user, password) = s
                        .split_once(':')
                        .with_context(|| format!("{} must hold user:password", p.display()))?;
                    return Ok(Credential::Basic {
                        user: user.into(),
                        password: password.into(),
                    });
                }
                if let Some(p) = &self.bearer_token_file {
                    return Ok(Credential::Bearer(read(p)?));
                }
                if let (Some(n), Some(p)) = (&self.header_name, &self.header_file) {
                    let name = HeaderName::try_from(n.as_str()).with_context(|| format!("header name {n:?}"))?;
                    let mut value = HeaderValue::try_from(read(p)?).context("header value")?;
                    value.set_sensitive(true);
                    return Ok(Credential::Header(name, value));
                }
                Ok(Credential::None)
            }
        }
    };
}

/// A secret file's contents without surrounding whitespace.
fn read_secret(p: &std::path::Path) -> Result<String> {
    let s = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    let s = s.trim();
    if s.is_empty() {
        bail!("{} is empty", p.display());
    }
    Ok(s.to_string())
}

credential_args!(
    UpstreamCredentialArgs,
    "upstream",
    "YOINK_UPSTREAM_BASIC_AUTH_FILE",
    "YOINK_UPSTREAM_BEARER_TOKEN_FILE",
    "YOINK_UPSTREAM_HEADER_NAME",
    "YOINK_UPSTREAM_HEADER_FILE"
);
credential_args!(
    TargetCredentialArgs,
    "target",
    "YOINK_TARGET_BASIC_AUTH_FILE",
    "YOINK_TARGET_BEARER_TOKEN_FILE",
    "YOINK_TARGET_HEADER_NAME",
    "YOINK_TARGET_HEADER_FILE"
);

/// Options of the `http` backend.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Binary cache to copy from (or a proxy of it).
    #[arg(long, env = "YOINK_UPSTREAM_URL", default_value = "https://cache.nixos.org")]
    pub upstream_url: String,
    #[command(flatten)]
    pub upstream_auth: UpstreamCredentialArgs,
    /// Binary cache to copy to; must accept GET and PUT of
    /// `<url>/<hash>.narinfo` and the NAR files.
    #[arg(long, env = "YOINK_TARGET_URL")]
    pub target_url: String,
    #[command(flatten)]
    pub target_auth: TargetCredentialArgs,
    /// Where NARs go on the target.
    #[arg(long, env = "YOINK_NAR_LAYOUT", value_enum, default_value_t = NarLayout::Nar)]
    pub nar_layout: NarLayout,
    /// File holding a Nix signing key (`name:base64`); its signature is added
    /// to each narinfo, next to the upstream ones.
    #[arg(long, env = "YOINK_SIGNING_KEY_FILE", value_name = "FILE")]
    pub signing_key_file: Option<PathBuf>,
    /// Store paths transferred at once.
    #[arg(long, env = "YOINK_CONCURRENCY", default_value_t = 16)]
    pub concurrency: usize,
    /// Retries per step for transient failures.
    #[arg(long, env = "YOINK_RETRIES", default_value_t = 5)]
    pub retries: u32,
    /// Priority written to the target's nix-cache-info if it has none.
    #[arg(long, env = "YOINK_CACHE_PRIORITY", default_value_t = 30)]
    pub cache_priority: u32,
    /// Don't create nix-cache-info on the target.
    #[arg(long, env = "YOINK_NO_CACHE_INFO")]
    pub no_cache_info: bool,
    /// Per-request timeout in seconds, without data flowing.
    #[arg(long = "http-timeout", env = "YOINK_HTTP_TIMEOUT", default_value_t = 120)]
    pub timeout: u64,
}

impl Args {
    pub fn config(&self) -> Result<Config> {
        Ok(Config {
            upstream: Endpoint::new(&self.upstream_url, self.upstream_auth.credential()?),
            target: Endpoint::new(&self.target_url, self.target_auth.credential()?),
            layout: self.nar_layout,
            signer: self
                .signing_key_file
                .as_deref()
                .map(|p| Signer::parse(&read_secret(p)?).with_context(|| format!("in {}", p.display())))
                .transpose()?,
            concurrency: self.concurrency.max(1),
            retries: self.retries,
            retry_delay: Duration::from_secs(1),
            cache_info_priority: (!self.no_cache_info).then_some(self.cache_priority),
            timeout: Duration::from_secs(self.timeout),
        })
    }
}

/// Whether a failed step may succeed if retried.
#[derive(Debug)]
enum Failure {
    Transient(anyhow::Error),
    Permanent(anyhow::Error),
}

impl From<reqwest::Error> for Failure {
    fn from(e: reqwest::Error) -> Self {
        match e.status() {
            Some(s) if !retryable(s) => Failure::Permanent(e.into()),
            _ => Failure::Transient(e.into()),
        }
    }
}

fn retryable(s: reqwest::StatusCode) -> bool {
    s.is_server_error()
        || s == reqwest::StatusCode::TOO_MANY_REQUESTS
        || s == reqwest::StatusCode::REQUEST_TIMEOUT
}

/// Fail on a non-success status, classifying it.
async fn ok(what: &str, r: reqwest::Response) -> Result<reqwest::Response, Failure> {
    let s = r.status();
    if s.is_success() {
        return Ok(r);
    }
    let body = r.text().await.unwrap_or_default();
    let body: String = body.chars().take(300).collect();
    let e = anyhow!(
        "{what}: HTTP {s}{}",
        if body.is_empty() {
            String::new()
        } else {
            format!(": {body}")
        }
    );
    Err(if retryable(s) {
        Failure::Transient(e)
    } else {
        Failure::Permanent(e)
    })
}

struct Inner {
    cfg: Config,
    http: reqwest::Client,
    slots: tokio::sync::Semaphore,
}

pub struct HttpMirror {
    inner: Arc<Inner>,
    jobs: JobTable<StoreHash>,
}

impl HttpMirror {
    /// Set up the backend, creating the target's `nix-cache-info` if needed.
    pub async fn new(cfg: Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("yoink/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(cfg.timeout)
            .build()?;
        let inner = Arc::new(Inner {
            slots: tokio::sync::Semaphore::new(cfg.concurrency),
            cfg,
            http,
        });
        if let Some(p) = inner.cfg.cache_info_priority {
            inner.ensure_cache_info(p).await?;
        }
        Ok(HttpMirror {
            inner,
            jobs: JobTable::default(),
        })
    }

    pub async fn from_args(a: &Args) -> Result<Self> {
        Self::new(a.config()?).await
    }
}

impl Mirror for HttpMirror {
    fn phases(&self) -> Vec<PhaseDef> {
        vec![
            PhaseDef::new(QUEUED, "Queued", 'Q'),
            PhaseDef::new(CHECKING, "Checking target", 'C'),
            PhaseDef::new(NARINFO, "Fetching narinfo", 'N'),
            PhaseDef::new(TRANSFER, "Transferring NAR", 'T'),
            PhaseDef::new(PUBLISH, "Publishing narinfo", 'P'),
        ]
    }

    fn mirror(&self, purl: &Purl) -> Result<Job> {
        let target = purl.nix_target()?;
        let inner = self.inner.clone();
        Ok(self.jobs.get_or_start(
            target.hash,
            QUEUED,
            move |r| async move { inner.run(target, r).await },
        ))
    }
}

impl Inner {
    fn get(&self, e: &Endpoint, path: &str) -> reqwest::RequestBuilder {
        e.credential.apply(self.http.get(format!("{}/{path}", e.url)))
    }

    fn put(&self, path: &str) -> reqwest::RequestBuilder {
        let t = &self.cfg.target;
        t.credential.apply(self.http.put(format!("{}/{path}", t.url)))
    }

    async fn ensure_cache_info(&self, priority: u32) -> Result<()> {
        let t = &self.cfg.target.url;
        let r = self
            .get(&self.cfg.target, "nix-cache-info")
            .send()
            .await
            .with_context(|| format!("connecting to the target cache {t}"))?;
        match r.status() {
            s if s.is_success() => return Ok(()),
            reqwest::StatusCode::NOT_FOUND => {}
            s => bail!("checking {t}/nix-cache-info: HTTP {s}"),
        }
        let body = format!("StoreDir: /nix/store\nWantMassQuery: 1\nPriority: {priority}\n");
        let r = self
            .put("nix-cache-info")
            .header(reqwest::header::CONTENT_TYPE, "text/x-nix-cache-info")
            .body(body)
            .send()
            .await?;
        if !r.status().is_success() {
            bail!("creating {t}/nix-cache-info: HTTP {}", r.status());
        }
        tracing::info!(target = %t, "created nix-cache-info");
        Ok(())
    }

    /// Run `step` until it succeeds, fails permanently or runs out of retries.
    async fn retry<T, F, Fut>(&self, r: &JobReporter, what: &str, mut step: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, Failure>>,
    {
        let mut delay = self.cfg.retry_delay;
        let mut attempt = 0;
        loop {
            match step().await {
                Ok(v) => return Ok(v),
                Err(Failure::Permanent(e)) => return Err(e.context(what.to_string())),
                Err(Failure::Transient(e)) if attempt >= self.cfg.retries => {
                    return Err(e.context(format!("{what} (gave up after {} attempts)", attempt + 1)));
                }
                Err(Failure::Transient(e)) => {
                    attempt += 1;
                    r.warn(format!(
                        "{what}: {e:#}; retrying in {}s ({attempt}/{})",
                        delay.as_secs_f32(),
                        self.cfg.retries
                    ));
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(30));
                }
            }
        }
    }

    async fn run(&self, target: NixTarget, r: JobReporter) -> Result<()> {
        let _slot = self.slots.acquire().await?;
        let hash = target.hash;
        r.phase(CHECKING);
        let present = self
            .retry(&r, "checking the target", || async {
                let resp = self
                    .get(&self.cfg.target, &format!("{hash}.narinfo"))
                    .send()
                    .await?;
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(false);
                }
                ok("checking the target", resp).await.map(|_| true)
            })
            .await?;
        if present {
            r.info("already in the target cache");
            return Ok(());
        }

        r.phase(NARINFO);
        let text = self
            .retry(&r, "fetching the narinfo", || async {
                let resp = self
                    .get(&self.cfg.upstream, &format!("{hash}.narinfo"))
                    .send()
                    .await?;
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    return Err(Failure::Permanent(anyhow!(
                        "{} is not in the upstream cache {}",
                        target.path(),
                        self.cfg.upstream.url
                    )));
                }
                Ok(ok("fetching the narinfo", resp).await?.text().await?)
            })
            .await?;
        let ni = Narinfo::parse(&text).context("parsing the upstream narinfo")?;
        if ni.store_path.hash != hash {
            bail!("the upstream narinfo is for {}", ni.store_path);
        }
        if let Some(n) = &target.name
            && *n != ni.store_path.name
        {
            bail!(
                "{} is {} upstream; the requested name doesn't match",
                target.path(),
                ni.store_path
            );
        }
        r.info(format!(
            "{} ({} compressed, {} unpacked)",
            ni.store_path,
            ni.file_size.map(human).unwrap_or_else(|| "?".into()),
            human(ni.nar_size)
        ));

        let file = ni.url.rsplit('/').next().unwrap_or(&ni.url).to_string();
        if file.is_empty() || file.contains("..") {
            bail!("unusable NAR URL {:?}", ni.url);
        }
        let key = match self.cfg.layout {
            NarLayout::Nar => format!("nar/{file}"),
            NarLayout::Flat => file,
        };
        r.phase(TRANSFER);
        self.retry(&r, "transferring the NAR", || self.transfer(&ni, &key, &r))
            .await?;

        r.phase(PUBLISH);
        let sigs = match &self.cfg.signer {
            Some(s) => vec![s.sign(&ni)?],
            None => vec![],
        };
        let body = ni.render(&key, &sigs);
        self.retry(&r, "publishing the narinfo", || async {
            let resp = self
                .put(&format!("{hash}.narinfo"))
                .header(reqwest::header::CONTENT_TYPE, "text/x-nix-narinfo")
                .body(body.clone())
                .send()
                .await?;
            ok("publishing the narinfo", resp).await.map(|_| ())
        })
        .await?;
        Ok(())
    }

    /// Stream the NAR from upstream into a PUT to the target, checking its
    /// size and hash.
    async fn transfer(&self, ni: &Narinfo, key: &str, r: &JobReporter) -> Result<(), Failure> {
        let up = &self.cfg.upstream;
        let rb = if ni.url.contains("://") {
            let rb = self.http.get(&ni.url);
            // Only send our upstream credentials to the upstream itself.
            if ni.url.starts_with(&format!("{}/", up.url)) {
                up.credential.apply(rb)
            } else {
                rb
            }
        } else {
            self.get(up, &ni.url)
        };
        let resp = ok("downloading the NAR", rb.send().await?).await?;
        let size = ni.file_size.or(resp.content_length());
        let max = size.unwrap_or(0);
        r.progress(0, max, Some(Unit::Bytes));

        let (mut tx, rx) = futures::channel::mpsc::channel::<std::io::Result<Bytes>>(4);
        let mut put = self
            .put(key)
            .header(reqwest::header::CONTENT_TYPE, "application/x-nix-nar");
        if let Some(n) = size {
            put = put.header(reqwest::header::CONTENT_LENGTH, n);
        }
        let upload = put.body(reqwest::Body::wrap_stream(rx)).send();

        let pump = async {
            let mut hasher = sha2::Sha256::new();
            let mut n = 0u64;
            let mut body = resp.bytes_stream();
            while let Some(chunk) = body.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.send(Err(std::io::Error::other("upstream failed"))).await;
                        return Err(Failure::Transient(anyhow!(e).context("downloading the NAR")));
                    }
                };
                hasher.update(&chunk);
                n += chunk.len() as u64;
                r.progress(n, max.max(n), Some(Unit::Bytes));
                if size.is_some_and(|s| n > s) {
                    let _ = tx.send(Err(std::io::Error::other("too long"))).await;
                    return Err(Failure::Transient(anyhow!("the NAR is longer than its FileSize")));
                }
                if tx.send(Ok(chunk)).await.is_err() {
                    // The upload ended early; its result says why.
                    return Ok((n, hasher.finalize()));
                }
            }
            drop(tx);
            Ok((n, hasher.finalize()))
        };
        let (pumped, uploaded) = tokio::join!(pump, upload);
        let (n, digest) = pumped?;
        ok("uploading the NAR", uploaded?).await?;
        if let Some(s) = ni.file_size
            && n != s
        {
            return Err(Failure::Transient(anyhow!("got {n} bytes of NAR, expected {s}")));
        }
        let expected = match (&ni.file_hash, ni.compression.as_deref()) {
            (Some(h), _) => Some(h.as_str()),
            (None, None | Some("none")) => Some(ni.nar_hash.as_str()),
            (None, _) => None,
        };
        match expected.and_then(|h| h.strip_prefix("sha256:")) {
            Some(h) if h.len() == 52 => {
                let got = nixbase32::encode(&digest);
                if got != h {
                    return Err(Failure::Transient(anyhow!(
                        "NAR hash mismatch: got sha256:{got}, expected sha256:{h}"
                    )));
                }
            }
            _ => r.warn("no usable FileHash in the narinfo; the NAR's hash was not checked"),
        }
        Ok(())
    }
}

fn human(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", units[u])
}

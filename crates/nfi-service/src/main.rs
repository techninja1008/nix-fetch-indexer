use anyhow::{Context, Result, bail};
use clap::Parser;
use nfi_service::auth::{AdminAuth, ServiceAuth};
use nfi_service::{AppState, api, connect, jobs, search::Meili};
use nfi_service_common::{OidcArgs, init_tracing, shutdown_signal};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "nfi-service", version, about = "nix-fetch-indexer query service")]
struct Cli {
    /// Postgres connection URL.
    #[arg(long, env = "NFI_DATABASE_URL", required_unless_present = "embedded_postgres")]
    database_url: Option<String>,
    /// Run a private PostgreSQL server as a child process, keeping its data
    /// in this directory (single-node mode; the server must not run as root).
    #[arg(long, env = "NFI_EMBEDDED_POSTGRES", conflicts_with = "database_url")]
    embedded_postgres: Option<PathBuf>,
    /// Directory with the PostgreSQL server binaries for --embedded-postgres
    /// [default: pg_config --bindir, then the PATH]
    #[arg(long, env = "NFI_PG_BINDIR")]
    pg_bindir: Option<PathBuf>,
    #[arg(long, env = "NFI_DB_MAX_CONNECTIONS", default_value_t = 16)]
    db_max_connections: u32,
    /// Meilisearch URL (search endpoints are disabled without it).
    #[arg(long, env = "NFI_MEILI_URL")]
    meili_url: Option<String>,
    #[arg(long, env = "NFI_MEILI_KEY", hide_env_values = true)]
    meili_key: Option<String>,
    #[arg(long, env = "NFI_MEILI_INDEX", default_value = "packages")]
    meili_index: String,
    /// Run a private Meilisearch server as a child process, keeping its data
    /// in this directory. The index is rebuilt from the database whenever it
    /// is new or was written by another Meilisearch version.
    #[arg(long, env = "NFI_EMBEDDED_MEILISEARCH", conflicts_with_all = ["meili_url", "meili_key"])]
    embedded_meilisearch: Option<PathBuf>,
    /// meilisearch binary for --embedded-meilisearch [default: from the PATH]
    #[arg(long, env = "NFI_MEILISEARCH_BIN")]
    meilisearch_bin: Option<PathBuf>,
    /// Public (query) listener.
    #[arg(long, env = "NFI_LISTEN", default_value = "0.0.0.0:8080")]
    listen: String,
    /// Admin listener; keep it on a private interface.
    #[arg(long, env = "NFI_ADMIN_LISTEN", default_value = "127.0.0.1:8081")]
    admin_listen: String,
    /// File holding a static bearer token required on the admin listener.
    #[arg(long, env = "NFI_ADMIN_TOKEN_FILE", conflicts_with = "admin_oidc")]
    admin_token_file: Option<PathBuf>,
    #[command(flatten)]
    auth: AuthArgs,
    /// Do not run the background job worker on this replica.
    #[arg(long, env = "NFI_NO_WORKER")]
    no_worker: bool,
}

/// OIDC login (optional). With an issuer and client id, the public API
/// accepts ID tokens as bearer tokens; the admin listener does too with
/// --admin-oidc.
#[derive(clap::Args)]
struct AuthArgs {
    #[command(flatten)]
    oidc: OidcArgs,
    /// File with a CEL policy (over `claims`, `user`, `request`) that
    /// authenticated public API requests must pass [default: true].
    #[arg(long, env = "NFI_MAIN_AUTHZ_CEL_FILE", requires = "oidc_issuer")]
    main_authz_cel_file: Option<PathBuf>,
    /// Let requests without a token use the public API (the policy is not
    /// applied to them).
    #[arg(long, env = "NFI_ALLOW_ANONYMOUS", requires = "oidc_issuer")]
    allow_anonymous: bool,
    /// Authenticate the admin listener with OIDC instead of a static token.
    #[arg(
        long,
        env = "NFI_ADMIN_OIDC",
        requires_all = ["oidc_issuer", "admin_authz_cel_file"]
    )]
    admin_oidc: bool,
    /// File with the CEL policy admin requests must pass (with --admin-oidc).
    #[arg(long, env = "NFI_ADMIN_AUTHZ_CEL_FILE", requires = "admin_oidc")]
    admin_authz_cel_file: Option<PathBuf>,
}

async fn service_auth(admin_token_file: Option<&Path>, o: &AuthArgs) -> Result<ServiceAuth> {
    use nfi_auth_server::{Authenticator, Expr, read_trimmed};
    let admin = match (admin_token_file, o.admin_oidc) {
        (Some(_), true) => bail!("--admin-token-file and --admin-oidc are mutually exclusive"),
        (Some(f), false) => AdminAuth::Token(read_trimmed(f)?),
        (None, true) => AdminAuth::Oidc(Expr::from_file(
            o.admin_authz_cel_file
                .as_deref()
                .context("--admin-oidc requires --admin-authz-cel-file")?,
        )?),
        (None, false) => AdminAuth::Open,
    };
    let Some(settings) = o.oidc.settings()? else {
        return match admin {
            AdminAuth::Open => Ok(ServiceAuth::open()),
            AdminAuth::Token(t) => Ok(ServiceAuth::admin_token(&t)),
            AdminAuth::Oidc(_) => bail!("--admin-oidc requires --oidc-issuer and --oidc-client-id"),
        };
    };
    let main = Expr::from_file_or(o.main_authz_cel_file.as_deref(), "true")?;
    let authenticator = Authenticator::start(settings).await?;
    Ok(ServiceAuth::oidc(authenticator, main, o.allow_anonymous, admin))
}

/// Child processes owned by this service; stopped when dropped.
#[derive(Default)]
struct Embedded {
    #[cfg(feature = "embedded-postgres")]
    postgres: Option<nfi_embedded_postgres::EmbeddedPostgres>,
    #[cfg(feature = "embedded-meilisearch")]
    meilisearch: Option<nfi_embedded_meilisearch::EmbeddedMeilisearch>,
}

impl Embedded {
    /// Wait until an embedded server exits (never, without any), then say
    /// which and how. Checked once a second.
    async fn died(&mut self) -> String {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            #[cfg(feature = "embedded-postgres")]
            if let Some(p) = &mut self.postgres {
                match p.exited() {
                    Ok(Some(status)) => return format!("embedded PostgreSQL exited ({status})"),
                    Ok(None) => {}
                    Err(e) => tracing::warn!(error = %format!("{e:#}"), "checking embedded PostgreSQL"),
                }
            }
            #[cfg(feature = "embedded-meilisearch")]
            if let Some(m) = &mut self.meilisearch {
                match m.exited() {
                    Ok(Some(status)) => return format!("embedded Meilisearch exited ({status})"),
                    Ok(None) => {}
                    Err(e) => tracing::warn!(error = %format!("{e:#}"), "checking embedded Meilisearch"),
                }
            }
        }
    }

    fn stop(self) {
        // Stop search first, then the database.
        #[cfg(feature = "embedded-meilisearch")]
        if let Some(m) = self.meilisearch
            && let Err(e) = m.stop()
        {
            tracing::warn!(error = %format!("{e:#}"), "stopping embedded Meilisearch");
        }
        #[cfg(feature = "embedded-postgres")]
        if let Some(p) = self.postgres
            && let Err(e) = p.stop()
        {
            tracing::warn!(error = %format!("{e:#}"), "stopping embedded PostgreSQL");
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing("info,sqlx=warn");
    let cli: Cli = nfi_service_common::parse(|c| OidcArgs::env(c, "NFI_"));
    let mut embedded = Embedded::default();
    // Start embedded servers on the main thread: they are tied to the
    // lifetime of the spawning thread via PR_SET_PDEATHSIG.
    let res = run(cli, &mut embedded).await;
    embedded.stop();
    res
}

#[cfg_attr(
    not(all(feature = "embedded-postgres", feature = "embedded-meilisearch")),
    allow(unused_mut, unused_variables)
)]
async fn run(cli: Cli, embedded: &mut Embedded) -> Result<()> {
    let auth = Arc::new(service_auth(cli.admin_token_file.as_deref(), &cli.auth).await?);
    let database_url = match (&cli.database_url, &cli.embedded_postgres) {
        (Some(u), _) => u.clone(),
        #[cfg(feature = "embedded-postgres")]
        (None, Some(dir)) => {
            if cli.no_worker {
                bail!("--no-worker cannot be used with --embedded-postgres (no other replica can run jobs)");
            }
            let mut opts = nfi_embedded_postgres::Options::new(dir);
            opts.bin_dir = cli.pg_bindir.clone();
            let pg = nfi_embedded_postgres::EmbeddedPostgres::start(&opts)?;
            let url = pg.url();
            embedded.postgres = Some(pg);
            url
        }
        #[cfg(not(feature = "embedded-postgres"))]
        (None, Some(_)) => bail!("this build does not include the embedded-postgres feature"),
        (None, None) => bail!("--database-url or --embedded-postgres is required"),
    };

    let mut rebuild_search = false;
    let (meili_url, meili_key) = match (&cli.meili_url, &cli.embedded_meilisearch) {
        (Some(u), _) => (Some(u.clone()), cli.meili_key.clone()),
        #[cfg(feature = "embedded-meilisearch")]
        (None, Some(dir)) => {
            let mut opts = nfi_embedded_meilisearch::Options::new(dir);
            opts.binary = cli.meilisearch_bin.clone();
            let m = nfi_embedded_meilisearch::EmbeddedMeilisearch::start(&opts)?;
            rebuild_search = m.reset;
            let r = (Some(m.url.clone()), Some(m.master_key.clone()));
            embedded.meilisearch = Some(m);
            r
        }
        #[cfg(not(feature = "embedded-meilisearch"))]
        (None, Some(_)) => bail!("this build does not include the embedded-meilisearch feature"),
        (None, None) => (None, None),
    };

    let pool = connect(&database_url, cli.db_max_connections).await?;
    let meili = meili_url
        .as_deref()
        .map(|u| Meili::new(u, meili_key, &cli.meili_index))
        .transpose()?;
    if let Some(m) = &meili
        && let Err(e) = m.ensure_index().await
    {
        tracing::warn!(error = %format!("{e:#}"), "could not configure meilisearch index");
    }
    if rebuild_search {
        let id = jobs::enqueue(
            &pool,
            "reindex",
            serde_json::json!({"reason": "embedded meilisearch reset"}),
        )
        .await?;
        tracing::info!(
            job = id,
            "queued search rebuild for the new embedded Meilisearch database"
        );
    }
    let state = AppState {
        pool,
        meili,
        auth,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(3600))
            .build()?,
        poll_interval: Duration::from_secs(5),
    };
    let worker = (!cli.no_worker).then(|| {
        let id = format!("{}-{}", hostname(), std::process::id());
        tokio::spawn(jobs::worker(state.clone(), id))
    });
    let public = tokio::net::TcpListener::bind(&cli.listen).await?;
    let admin = tokio::net::TcpListener::bind(&cli.admin_listen).await?;
    tracing::info!(public = %public.local_addr()?, admin = %admin.local_addr()?, "listening");

    let (tx, rx) = tokio::sync::watch::channel(());
    let wait = |mut rx: tokio::sync::watch::Receiver<()>| async move {
        let _ = rx.changed().await;
    };
    let p = axum::serve(public, api::public_router(state.clone())).with_graceful_shutdown(wait(rx.clone()));
    let a = axum::serve(admin, api::admin_router(state.clone())).with_graceful_shutdown(wait(rx));
    let servers = async { tokio::try_join!(async { p.await }, async { a.await }) };
    tokio::pin!(servers);
    // An embedded server that dies leaves the service failing every request:
    // exit instead (non-zero), so a supervisor restarts the whole node.
    let mut failure = None;
    tokio::select! {
        r = &mut servers => { r?; }
        _ = shutdown_signal() => {
            let _ = tx.send(());
            // Give in-flight requests a moment to finish.
            let _ = tokio::time::timeout(Duration::from_secs(10), &mut servers).await;
        }
        msg = embedded.died() => {
            tracing::error!("{msg}; shutting down");
            let _ = tx.send(());
            let _ = tokio::time::timeout(Duration::from_secs(10), &mut servers).await;
            failure = Some(msg);
        }
    }
    // An interrupted job is picked up again (stale heartbeat) on the next start.
    if let Some(w) = worker {
        w.abort();
    }
    state.pool.close().await;
    match failure {
        Some(msg) => bail!(msg),
        None => Ok(()),
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "nfi".into())
}

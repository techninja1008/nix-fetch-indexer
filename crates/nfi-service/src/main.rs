use anyhow::Result;
use clap::Parser;
use nfi_service::{AppState, api, connect, jobs, search::Meili};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "nfi-service", version, about = "nix-fetch-indexer query service")]
struct Cli {
    /// Postgres connection URL.
    #[arg(long, env = "NFI_DATABASE_URL")]
    database_url: String,
    #[arg(long, env = "NFI_DB_MAX_CONNECTIONS", default_value_t = 16)]
    db_max_connections: u32,
    /// Meilisearch URL (search endpoints are disabled without it).
    #[arg(long, env = "NFI_MEILI_URL")]
    meili_url: Option<String>,
    #[arg(long, env = "NFI_MEILI_KEY", hide_env_values = true)]
    meili_key: Option<String>,
    #[arg(long, env = "NFI_MEILI_INDEX", default_value = "packages")]
    meili_index: String,
    /// Public (query) listener.
    #[arg(long, env = "NFI_LISTEN", default_value = "0.0.0.0:8080")]
    listen: String,
    /// Admin listener; keep it on a private interface.
    #[arg(long, env = "NFI_ADMIN_LISTEN", default_value = "127.0.0.1:8081")]
    admin_listen: String,
    /// Optional static bearer token required on the admin listener.
    #[arg(long, env = "NFI_ADMIN_TOKEN", hide_env_values = true)]
    admin_token: Option<String>,
    /// Do not run the background job worker on this replica.
    #[arg(long, env = "NFI_NO_WORKER")]
    no_worker: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sqlx=warn".into()),
        )
        .init();
    let cli = Cli::parse();
    let pool = connect(&cli.database_url, cli.db_max_connections).await?;
    let meili = cli
        .meili_url
        .as_deref()
        .map(|u| Meili::new(u, cli.meili_key.clone(), &cli.meili_index))
        .transpose()?;
    if let Some(m) = &meili
        && let Err(e) = m.ensure_index().await
    {
        tracing::warn!(error = %format!("{e:#}"), "could not configure meilisearch index");
    }
    let state = AppState {
        pool,
        meili,
        admin_token: cli.admin_token.filter(|t| !t.is_empty()),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(3600))
            .build()?,
        poll_interval: Duration::from_secs(5),
    };
    if !cli.no_worker {
        let id = format!("{}-{}", hostname(), std::process::id());
        tokio::spawn(jobs::worker(state.clone(), id));
    }
    let public = tokio::net::TcpListener::bind(&cli.listen).await?;
    let admin = tokio::net::TcpListener::bind(&cli.admin_listen).await?;
    tracing::info!(public = %cli.listen, admin = %cli.admin_listen, "listening");
    let p = axum::serve(public, api::public_router(state.clone()));
    let a = axum::serve(admin, api::admin_router(state));
    tokio::try_join!(async { p.await }, async { a.await })?;
    Ok(())
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "nfi".into())
}

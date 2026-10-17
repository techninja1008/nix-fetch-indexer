//! Query service for nix-fetch-indexer.
#![allow(clippy::type_complexity)]

pub mod api;
pub mod copy;
pub mod ingest;
pub mod jobs;
pub mod prune;
pub mod query;
pub mod search;

use std::time::Duration;

#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::PgPool,
    pub meili: Option<search::Meili>,
    pub admin_token: Option<String>,
    pub http: reqwest::Client,
    pub poll_interval: Duration,
}

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Connect to Postgres and apply migrations.
pub async fn connect(database_url: &str, max_connections: u32) -> anyhow::Result<sqlx::PgPool> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(30))
        .connect(database_url)
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

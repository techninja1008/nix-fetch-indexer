//! Pruning releases and garbage-collecting orphaned rows.

use crate::ingest::WRITE_LOCK;
use anyhow::Result;
use nfi_api::PruneRequest;
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

#[derive(Debug, Default, Serialize)]
pub struct PruneReport {
    pub releases: Vec<String>,
    pub store_paths_removed: u64,
    pub packages_removed: u64,
}

/// Delete store paths and packages that no release refers to any more.
pub async fn gc(tx: &mut Transaction<'_, Postgres>) -> Result<(u64, u64)> {
    // References live on the path rows, and a kept path never references a
    // removed one (a release's membership covers its whole closure).
    let paths = sqlx::query("DELETE FROM store_paths WHERE releases = '{}'")
        .execute(&mut **tx)
        .await?
        .rows_affected();
    let packages = sqlx::query(
        "DELETE FROM packages p WHERE NOT EXISTS (SELECT 1 FROM package_releases r WHERE r.package_id = p.id)",
    )
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok((paths, packages))
}

pub async fn prune(pool: &PgPool, req: &PruneRequest) -> Result<PruneReport> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(WRITE_LOCK)
        .execute(&mut *tx)
        .await?;
    let commit_patterns: Vec<String> = req
        .commits
        .iter()
        .map(|c| format!("{}%", c.to_lowercase()))
        .collect();
    let deleted: Vec<(i32, String)> = sqlx::query_as(
        "DELETE FROM releases WHERE name = ANY($1) OR channel = ANY($2) OR commit LIKE ANY($3) RETURNING id, name",
    )
    .bind(&req.releases)
    .bind(&req.channels)
    .bind(&commit_patterns)
    .fetch_all(&mut *tx)
    .await?;
    let (ids, names): (Vec<i32>, Vec<String>) = deleted.into_iter().unzip();
    if !ids.is_empty() {
        // Drop the pruned releases from store path membership.
        sqlx::query(
            "UPDATE store_paths
                SET releases = ARRAY(SELECT x FROM unnest(releases) x WHERE x <> ALL($1) ORDER BY x)
              WHERE releases && $1",
        )
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    }
    let (paths, packages) = if names.is_empty() {
        (0, 0)
    } else {
        gc(&mut tx).await?
    };
    tx.commit().await?;
    Ok(PruneReport {
        releases: names,
        store_paths_removed: paths,
        packages_removed: packages,
    })
}

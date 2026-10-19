//! Pruning releases and garbage-collecting orphaned rows.

use crate::ingest::WRITE_LOCK;
use anyhow::Result;
use nfi_api::PruneRequest;
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

#[derive(Debug, Default, Serialize)]
pub struct PruneReport {
    /// Releases removed entirely.
    pub releases: Vec<String>,
    /// `<release> <closure type>` for closures removed from kept releases.
    pub closures: Vec<String>,
    pub store_paths_removed: u64,
    pub packages_removed: u64,
}

/// Delete store paths and packages that no closure or release refers to
/// any more.
pub async fn gc(tx: &mut Transaction<'_, Postgres>) -> Result<(u64, u64)> {
    // References live on the path rows, and a kept path never references a
    // removed one (a closure's membership covers its whole closure).
    let paths = sqlx::query("DELETE FROM store_paths WHERE closures = '{}'")
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
    let selected = "SELECT id FROM releases WHERE name = ANY($1) OR channel = ANY($2) OR commit LIKE ANY($3)";
    // Closures being removed: all of the selected releases' closures (cascaded
    // from the release rows), or only those of the requested types.
    let removed: Vec<(i32, String, String)> = sqlx::query_as(&format!(
        "SELECT c.id, r.name, t.name FROM closures c
           JOIN releases r ON r.id = c.release_id
           JOIN closure_types t ON t.id = c.type_id
          WHERE c.release_id IN ({selected}) AND (cardinality($4::text[]) = 0 OR t.name = ANY($4))"
    ))
    .bind(&req.releases)
    .bind(&req.channels)
    .bind(&commit_patterns)
    .bind(&req.closure_types)
    .fetch_all(&mut *tx)
    .await?;
    let closure_ids: Vec<i32> = removed.iter().map(|(id, _, _)| *id).collect();
    let mut names = Vec::new();
    let mut closures = Vec::new();
    if req.closure_types.is_empty() {
        names = sqlx::query_scalar(&format!(
            "DELETE FROM releases WHERE id IN ({selected}) RETURNING name"
        ))
        .bind(&req.releases)
        .bind(&req.channels)
        .bind(&commit_patterns)
        .fetch_all(&mut *tx)
        .await?;
    } else {
        sqlx::query("DELETE FROM closures WHERE id = ANY($1)")
            .bind(&closure_ids)
            .execute(&mut *tx)
            .await?;
        // A release left without closures no longer has the closure dataset.
        sqlx::query(
            "DELETE FROM release_datasets d WHERE d.dataset = 'closure'
                AND NOT EXISTS (SELECT 1 FROM closures c WHERE c.release_id = d.release_id)",
        )
        .execute(&mut *tx)
        .await?;
        closures = removed.iter().map(|(_, r, t)| format!("{r} {t}")).collect();
    }
    if !closure_ids.is_empty() {
        // Drop the removed closures from store path membership.
        sqlx::query(
            "UPDATE store_paths
                SET closures = ARRAY(SELECT x FROM unnest(closures) x WHERE x <> ALL($1) ORDER BY x)
              WHERE closures && $1",
        )
        .bind(&closure_ids)
        .execute(&mut *tx)
        .await?;
    }
    let (paths, packages) = if names.is_empty() && closure_ids.is_empty() {
        (0, 0)
    } else {
        gc(&mut tx).await?
    };
    tx.commit().await?;
    Ok(PruneReport {
        releases: names,
        closures,
        store_paths_removed: paths,
        packages_removed: packages,
    })
}

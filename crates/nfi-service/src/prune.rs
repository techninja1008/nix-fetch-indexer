//! Pruning releases and garbage-collecting orphaned rows.

use crate::ingest::WRITE_LOCK;
use anyhow::{Result, bail};
use nfi_api::PruneRequest;
use nfi_interchange::ClosureType;
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
    // Commits become `LIKE` prefix patterns: an empty one would match every
    // release, and `%`/`_` would act as wildcards.
    if let Some(c) = req.invalid_commit() {
        bail!("invalid commit {c:?}: expected a non-empty hex commit prefix");
    }
    let types = req
        .closure_types
        .iter()
        .map(|s| s.parse::<ClosureType>().map_err(anyhow::Error::msg))
        .collect::<Result<Vec<_>>>()?;
    let sel_evals: Vec<&str> = types.iter().map(|t| t.eval_type.as_str()).collect();
    let sel_names: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();
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
    // from the release rows), or only those of the given closure types
    // (`<eval type>[:<closure type>]`).
    let removed: Vec<(i32, String, String, String)> = sqlx::query_as(&format!(
        "SELECT c.id, r.name, et.name, ct.name FROM closures c
           JOIN evals e ON e.id = c.eval_id
           JOIN eval_types et ON et.id = e.type_id
           JOIN closure_types ct ON ct.id = c.type_id
           JOIN releases r ON r.id = e.release_id
          WHERE e.release_id IN ({selected})
            AND (cardinality($4::text[]) = 0
                 OR EXISTS (SELECT 1 FROM unnest($4::text[], $5::text[]) s(eval_type, name)
                             WHERE s.eval_type = et.name AND s.name = ct.name))"
    ))
    .bind(&req.releases)
    .bind(&req.channels)
    .bind(&commit_patterns)
    .bind(&sel_evals)
    .bind(&sel_names)
    .fetch_all(&mut *tx)
    .await?;
    let closure_ids: Vec<i32> = removed.iter().map(|(id, ..)| *id).collect();
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
        // Evaluations left with neither closures nor known errors go too,
        // and a release left without closures loses the closure dataset.
        sqlx::query(
            "DELETE FROM evals e WHERE NOT e.errors_indexed
                AND NOT EXISTS (SELECT 1 FROM closures c WHERE c.eval_id = e.id)",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM release_datasets d WHERE d.dataset = 'closure'
                AND NOT EXISTS (SELECT 1 FROM closures c JOIN evals e ON e.id = c.eval_id
                                 WHERE e.release_id = d.release_id)",
        )
        .execute(&mut *tx)
        .await?;
        closures = removed
            .iter()
            .map(|(_, r, e, n)| format!("{r} {}", ClosureType::new(e, n)))
            .collect();
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

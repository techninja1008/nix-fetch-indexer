//! Pruning releases and garbage-collecting orphaned rows.

use crate::ingest::WRITE_LOCK;
use anyhow::{Result, bail};
use nfi_api::{PruneReport, PruneRequest};
use nfi_interchange::ClosureType;
use sqlx::{PgExecutor, PgPool, Postgres, Transaction};

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

/// Shortest commit prefix prune accepts.
pub const MIN_COMMIT_PREFIX: usize = 8;

/// The first commit of `req` that is not a valid commit prefix, if any.
/// Commits select releases by prefix, so a short one could select several.
pub fn invalid_commit(req: &PruneRequest) -> Option<&str> {
    req.commits
        .iter()
        .map(String::as_str)
        .find(|c| !is_prune_commit_prefix(c))
}

/// Whether `s` is a commit prefix prune accepts: at least
/// [`MIN_COMMIT_PREFIX`] hex digits (either case).
pub fn is_prune_commit_prefix(s: &str) -> bool {
    s.len() >= MIN_COMMIT_PREFIX && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Why the commits of `req` can't be used, if they can't: one is not a
/// valid prefix, or matches more than one release. A prefix matching none
/// selects nothing, like an unknown release or channel.
pub async fn commit_error(db: impl PgExecutor<'_>, req: &PruneRequest) -> Result<Option<String>> {
    if let Some(c) = invalid_commit(req) {
        return Ok(Some(format!(
            "invalid commit {c:?}: expected a hex commit prefix of at least {MIN_COMMIT_PREFIX} characters"
        )));
    }
    let ambiguous: Option<(String, i64)> = sqlx::query_as(
        "SELECT c, count(*) FROM unnest($1::text[]) WITH ORDINALITY u(c, n)
           JOIN releases r ON r.commit LIKE lower(c) || '%'
          GROUP BY c, n HAVING count(*) > 1 ORDER BY n LIMIT 1",
    )
    .bind(&req.commits)
    .fetch_optional(db)
    .await?;
    Ok(ambiguous.map(|(c, n)| format!("commit prefix {c:?} is ambiguous: it matches {n} releases")))
}

pub async fn prune(pool: &PgPool, req: &PruneRequest) -> Result<PruneReport> {
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
    // Commits become `LIKE` prefix patterns (`%`/`_` would act as
    // wildcards). Checked under the lock: the request was checked when it
    // was queued, but a release may have been ingested since.
    if let Some(e) = commit_error(&mut *tx, req).await? {
        bail!(e);
    }
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
        search_documents: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_request_rejects_short_or_non_hex_commits() {
        let req = |c: &str| PruneRequest {
            commits: vec!["abcdef12".into(), c.into()],
            ..Default::default()
        };
        assert_eq!(invalid_commit(&req("DEADbeef")), None);
        for bad in [
            "",
            "%",
            "abcdef1",
            "abcdef1_",
            "abcdef1%",
            " abcdef12",
            "xyzxyzxy",
        ] {
            assert_eq!(invalid_commit(&req(bad)), Some(bad));
        }
    }
}

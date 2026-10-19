//! Consistency checks for the closure index.
//!
//! Release membership and references are arrays on `store_paths` rows, which
//! Postgres cannot enforce with foreign keys. Ingest and prune maintain the
//! invariants below; this check verifies them.

use anyhow::Result;
use nfi_api::{CheckIssue, CheckReport};
use sqlx::PgPool;

const EXAMPLES: i64 = 10;

struct Check {
    kind: &'static str,
    description: &'static str,
    /// `FROM ... WHERE ...` selecting the violations.
    from: &'static str,
    /// Text describing one violation, for examples.
    example: &'static str,
}

const CHECKS: &[Check] = &[
    Check {
        kind: "dangling_refs",
        description: "references to store path ids that do not exist",
        from: "FROM store_paths s, unnest(s.refs) x
               WHERE NOT EXISTS (SELECT 1 FROM store_paths t WHERE t.id = x)",
        example: "format('%s-%s references missing id %s', s.hash, s.name, x)",
    },
    Check {
        kind: "unknown_closures",
        description: "membership in closure ids that do not exist",
        from: "FROM store_paths s, unnest(s.closures) x
               WHERE NOT EXISTS (SELECT 1 FROM closures c WHERE c.id = x)",
        example: "format('%s-%s is in missing closure id %s', s.hash, s.name, x)",
    },
    Check {
        kind: "orphan_paths",
        description: "store paths that belong to no closure",
        from: "FROM store_paths s WHERE s.closures = '{}'",
        example: "format('%s-%s', s.hash, s.name)",
    },
    Check {
        kind: "open_closures",
        description: "references to paths missing from a closure the referrer belongs to",
        from: "FROM store_paths s, unnest(s.refs) x
               JOIN store_paths t ON t.id = x
               WHERE NOT t.closures @> s.closures",
        example: "format('%s-%s (closures %s) references %s-%s (closures %s)',
                         s.hash, s.name, s.closures, t.hash, t.name, t.closures)",
    },
];

/// Run all checks in one read-only snapshot.
pub async fn check(pool: &PgPool) -> Result<CheckReport> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let (store_paths, references): (i64, Option<i64>) =
        sqlx::query_as("SELECT count(*), sum(cardinality(refs))::bigint FROM store_paths")
            .fetch_one(&mut *tx)
            .await?;
    let releases: i64 = sqlx::query_scalar("SELECT count(*) FROM releases")
        .fetch_one(&mut *tx)
        .await?;
    let closures: i64 = sqlx::query_scalar("SELECT count(*) FROM closures")
        .fetch_one(&mut *tx)
        .await?;
    let mut issues = Vec::new();
    for c in CHECKS {
        // A plain count can use parallel workers; examples are only fetched
        // when something was found.
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) {}", c.from))
            .fetch_one(&mut *tx)
            .await?;
        if count == 0 {
            continue;
        }
        tracing::warn!(kind = c.kind, count, "index inconsistency found");
        let examples: Vec<String> =
            sqlx::query_scalar(&format!("SELECT {} {} LIMIT {EXAMPLES}", c.example, c.from))
                .fetch_all(&mut *tx)
                .await?;
        issues.push(CheckIssue {
            kind: c.kind.into(),
            description: c.description.into(),
            count: count as u64,
            examples,
        });
    }
    tx.commit().await?;
    Ok(CheckReport {
        ok: issues.is_empty(),
        store_paths: store_paths as u64,
        references: references.unwrap_or(0) as u64,
        releases: releases as u64,
        closures: closures as u64,
        issues,
    })
}

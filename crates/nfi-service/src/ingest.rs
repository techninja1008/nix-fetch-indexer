//! Ingesting interchange bundles into Postgres.
//!
//! Ingest is purely additive: entries are immutable (a store path's
//! references never change, a release's contents never change), so every
//! insert is `ON CONFLICT DO NOTHING` and re-ingesting is a no-op. Nothing is
//! deleted here; removal only happens through pruning.

use crate::copy::{CopyBuf, copy_in};
use anyhow::{Context, Result};
use nfi_interchange::{Bundle, Dataset};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::BTreeSet;

/// Advisory lock key serialising all writers (ingest, prune).
pub const WRITE_LOCK: i64 = 0x6e66_6900;

#[derive(Debug, Default, Serialize)]
pub struct IngestReport {
    pub releases: Vec<String>,
    /// `<release> <closure type>` for each closure ingested.
    pub closures: Vec<String>,
    pub store_paths: usize,
    /// Store paths not previously in the database.
    pub new_store_paths: usize,
    pub references: usize,
    pub missing: usize,
    pub packages: usize,
    pub outpaths: usize,
    pub programs: usize,
    /// Attributes whose search documents need refreshing.
    #[serde(skip)]
    pub touched_attrs: BTreeSet<String>,
}

pub fn package_key(p: &nfi_interchange::Package) -> String {
    let mut p = p.clone();
    p.releases = Default::default();
    let json = serde_json::to_vec(&p).expect("serialisable");
    hex::encode(&Sha256::digest(&json)[..16])
}

async fn exec(tx: &mut Transaction<'_, Postgres>, sql: &str) -> Result<u64> {
    Ok(sqlx::query(sql)
        .execute(&mut **tx)
        .await
        .with_context(|| sql.to_string())?
        .rows_affected())
}

pub async fn ingest(
    pool: &PgPool,
    bundle: &Bundle,
    progress: &(dyn Fn(String) + Sync),
) -> Result<IngestReport> {
    bundle
        .validate()
        .map_err(anyhow::Error::msg)
        .context("invalid bundle")?;
    let mut report = IngestReport::default();
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(WRITE_LOCK)
        .execute(&mut *tx)
        .await?;

    // Releases.
    let mut ids: Vec<i32> = Vec::with_capacity(bundle.releases.len());
    for r in &bundle.releases {
        sqlx::query(
            "INSERT INTO releases (name, commit, channel, series, url) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT DO NOTHING",
        )
        .bind(&r.name)
        .bind(&r.commit)
        .bind(&r.channel)
        .bind(&r.series)
        .bind(&r.url)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("inserting release {}", r.name))?;
        let (id, commit): (i32, String) = sqlx::query_as("SELECT id, commit FROM releases WHERE name = $1")
            .bind(&r.name)
            .fetch_optional(&mut *tx)
            .await?
            .with_context(|| {
                format!(
                    "release {}: commit {} already belongs to another release",
                    r.name, r.commit
                )
            })?;
        if commit != r.commit {
            anyhow::bail!(
                "release {} is already indexed with commit {commit}, not {}",
                r.name,
                r.commit
            );
        }
        ids.push(id);
        report.releases.push(r.name.clone());
    }
    // Releases carrying each dataset whose section is present.
    let with = |d: Dataset| -> Vec<i32> {
        let present = bundle.sections().contains(&d);
        bundle
            .releases
            .iter()
            .zip(&ids)
            .filter(|(r, _)| present && r.datasets.contains(&d))
            .map(|(_, id)| *id)
            .collect()
    };
    for d in Dataset::ALL {
        let rel = with(d);
        if rel.is_empty() {
            continue;
        }
        sqlx::query(
            "INSERT INTO release_datasets (release_id, dataset) SELECT unnest($1::int[]), $2 ON CONFLICT DO NOTHING",
        )
        .bind(&rel)
        .bind(d.as_str())
        .execute(&mut *tx)
        .await?;
    }
    // Bundle release indices whose release carries a dataset, as a mask
    // (O(1) per membership bit rather than a scan of the release list).
    let mask = |allowed: &[i32]| -> Vec<bool> { ids.iter().map(|id| allowed.contains(id)).collect() };
    let release_ids = |bits: &nfi_interchange::Bitset, mask: &[bool]| -> Vec<i32> {
        bits.iter()
            .filter(|&i| mask.get(i).copied().unwrap_or(false))
            .map(|i| ids[i])
            .collect()
    };

    // Closure.
    //
    // A store path's references never change, so they are only written when
    // the path is new to the database; existing paths just gain closure ids.
    if let Some(c) = &bundle.closure {
        // Database closure id of each of the bundle's closures (validate()
        // ensures their releases carry the closure dataset).
        let mut closure_ids: Vec<i32> = Vec::with_capacity(c.closures.len());
        for cl in &c.closures {
            let release_id = ids[cl.release as usize];
            sqlx::query("INSERT INTO closure_types (name) VALUES ($1) ON CONFLICT DO NOTHING")
                .bind(&cl.closure_type)
                .execute(&mut *tx)
                .await?;
            let id: i32 = sqlx::query_scalar(
                "INSERT INTO closures (release_id, type_id, skipped)
                 SELECT $1, t.id, $3 FROM closure_types t WHERE t.name = $2
                 ON CONFLICT (release_id, type_id) DO UPDATE
                    SET skipped = ARRAY(SELECT DISTINCT x FROM unnest(closures.skipped || EXCLUDED.skipped) x ORDER BY x)
                 RETURNING id",
            )
            .bind(release_id)
            .bind(&cl.closure_type)
            .bind(&cl.skipped)
            .fetch_one(&mut *tx)
            .await
            .with_context(|| format!("adding closure {}", cl.closure_type))?;
            closure_ids.push(id);
            report.closures.push(format!(
                "{} {}",
                bundle.releases[cl.release as usize].name, cl.closure_type
            ));
        }
        let to_ids = |bits: &nfi_interchange::Bitset| -> Vec<i32> {
            bits.iter().filter_map(|i| closure_ids.get(i).copied()).collect()
        };
        progress(format!(
            "closure: {} paths in {} closures",
            c.paths.len(),
            c.closures.len()
        ));
        exec(
            &mut tx,
            "CREATE TEMP TABLE t_paths (idx int, hash text, name text, nar_size bigint, file_size bigint,
               deriver text, closures int[], id bigint, existing int[]) ON COMMIT DROP",
        )
        .await?;
        exec(
            &mut tx,
            "CREATE TEMP TABLE t_refs (idx int, ref int) ON COMMIT DROP",
        )
        .await?;
        exec(
            &mut tx,
            "CREATE TEMP TABLE t_missing (hash text, name text, closure_id int) ON COMMIT DROP",
        )
        .await?;
        let (mut paths, mut refs, mut missing) = (CopyBuf::new(), CopyBuf::new(), CopyBuf::new());
        let mut closures_text = String::new();
        for (i, p) in c.paths.iter().enumerate() {
            let ids = to_ids(&p.closures);
            // Paths in no closure are not stored (they would be orphans). The
            // bundle's closures are closed (checked by validate()), so no
            // stored path references them.
            if ids.is_empty() {
                continue;
            }
            let hash = p.hash.to_string();
            closures_text.clear();
            closures_text.push('{');
            for (n, id) in ids.into_iter().enumerate() {
                if n > 0 {
                    closures_text.push(',');
                }
                closures_text.push_str(&id.to_string());
            }
            closures_text.push('}');
            paths
                .field(i as i64)
                .field(&hash)
                .field(&p.name)
                .field(p.nar_size)
                .field(p.file_size)
                .field(&p.deriver)
                .field(closures_text.as_str());
            paths.end_row();
            for &r in &p.references {
                refs.field(i as i64).field(r);
                refs.end_row();
            }
        }
        for m in &c.missing {
            let hash = m.hash.to_string();
            for id in to_ids(&m.closures) {
                missing.field(&hash).field(&m.name).field(id);
                missing.end_row();
            }
        }
        report.store_paths = paths.rows;
        report.references = refs.rows;
        report.missing = missing.rows;
        copy_in(
            &mut tx,
            "t_paths (idx, hash, name, nar_size, file_size, deriver, closures)",
            paths,
        )
        .await?;
        copy_in(&mut tx, "t_missing", missing).await?;
        exec(&mut tx, "CREATE INDEX ON t_paths (idx)").await?;
        exec(&mut tx, "ANALYZE t_paths").await?;

        // Resolve ids: existing paths keep theirs, new paths get fresh ids up
        // front (writers hold WRITE_LOCK, so nothing else inserts meanwhile)
        // so their references can be written in the same insert.
        progress("closure: resolving paths".into());
        exec(
            &mut tx,
            "UPDATE t_paths t SET id = s.id, existing = s.closures FROM store_paths s WHERE s.hash = t.hash",
        )
        .await?;
        let new_paths = sqlx::query(
            "UPDATE t_paths SET id = nextval(pg_get_serial_sequence('store_paths', 'id')) WHERE id IS NULL",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        report.new_store_paths = new_paths as usize;

        if new_paths > 0 {
            progress(format!("closure: inserting {new_paths} new paths"));
            copy_in(&mut tx, "t_refs", refs).await?;
            exec(
                &mut tx,
                "INSERT INTO store_paths (id, hash, name, nar_size, file_size, deriver, closures, refs)
                 SELECT t.id, t.hash, t.name, t.nar_size, t.file_size, t.deriver, t.closures,
                        COALESCE(r.refs, '{}')
                   FROM t_paths t
                   LEFT JOIN (SELECT r.idx, array_agg(b.id ORDER BY b.id) AS refs
                                FROM t_refs r
                                JOIN t_paths a ON a.idx = r.idx AND a.existing IS NULL
                                JOIN t_paths b ON b.idx = r.ref
                               GROUP BY r.idx) r ON r.idx = t.idx
                  WHERE t.existing IS NULL",
            )
            .await?;
        }

        progress("closure: adding closures to existing paths".into());
        exec(
            &mut tx,
            "UPDATE store_paths s
                SET closures = ARRAY(SELECT DISTINCT x FROM unnest(t.existing || t.closures) x ORDER BY x)
               FROM t_paths t
              WHERE t.existing IS NOT NULL AND NOT t.existing @> t.closures AND s.id = t.id",
        )
        .await?;
        exec(
            &mut tx,
            "INSERT INTO missing_paths (hash, name, closure_id) SELECT hash, name, closure_id FROM t_missing ON CONFLICT DO NOTHING",
        )
        .await?;
    }

    // Packages.
    if let Some(s) = &bundle.packages {
        let rel = with(Dataset::Packages);
        let rel_mask = mask(&rel);
        progress(format!("packages: {} entries", s.entries.len()));
        exec(
            &mut tx,
            "CREATE TEMP TABLE t_pkgs (key text, attr text, system text, name text, pname text, version text,
               description text, homepage text, license text, main_program text, outputs text, broken bool, unfree bool) ON COMMIT DROP",
        )
        .await?;
        exec(
            &mut tx,
            "CREATE TEMP TABLE t_pkg_rel (key text, release_id int) ON COMMIT DROP",
        )
        .await?;
        let (mut pk, mut pr) = (CopyBuf::new(), CopyBuf::new());
        for p in &s.entries {
            let key = package_key(p);
            let license = serde_json::to_string(&p.license)?;
            let outputs = serde_json::to_string(&p.outputs)?;
            pk.field(&key)
                .field(&p.attr)
                .field(&p.system)
                .field(&p.name)
                .field(&p.pname)
                .field(&p.version)
                .field(&p.description)
                .field(&p.homepage)
                .field(&license)
                .field(&p.main_program)
                .field(&outputs)
                .field(p.broken)
                .field(p.unfree);
            pk.end_row();
            for id in release_ids(&p.releases, &rel_mask) {
                pr.field(&key).field(id);
                pr.end_row();
            }
            report.touched_attrs.insert(p.attr.clone());
        }
        report.packages = pk.rows;
        copy_in(&mut tx, "t_pkgs", pk).await?;
        copy_in(&mut tx, "t_pkg_rel", pr).await?;
        exec(
            &mut tx,
            "INSERT INTO packages (key, attr, system, name, pname, version, description, homepage, license, main_program, outputs, broken, unfree)
             SELECT DISTINCT ON (key) key, attr, system, name, pname, version, description, homepage,
               ARRAY(SELECT jsonb_array_elements_text(license::jsonb)), main_program,
               ARRAY(SELECT jsonb_array_elements_text(outputs::jsonb)), broken, unfree
             FROM t_pkgs ON CONFLICT (key) DO NOTHING",
        )
        .await?;
        exec(
            &mut tx,
            "INSERT INTO package_releases (package_id, release_id)
             SELECT p.id, t.release_id FROM t_pkg_rel t JOIN packages p ON p.key = t.key ON CONFLICT DO NOTHING",
        )
        .await?;
    }

    // Out paths (joined onto package rows at query time by release/attr/system).
    if let Some(s) = &bundle.outpaths {
        let rel = with(Dataset::Outpaths);
        let rel_mask = mask(&rel);
        progress(format!("outpaths: {} entries", s.entries.len()));
        exec(
            &mut tx,
            "CREATE TEMP TABLE t_out (release_id int, attr text, system text, output text, path text, drv_path text) ON COMMIT DROP",
        )
        .await?;
        let mut buf = CopyBuf::new();
        for e in &s.entries {
            for id in release_ids(&e.releases, &rel_mask) {
                for (o, p) in &e.outputs {
                    buf.field(id)
                        .field(&e.attr)
                        .field(&e.system)
                        .field(o)
                        .field(p)
                        .field(&e.drv_path);
                    buf.end_row();
                }
            }
        }
        report.outpaths = buf.rows;
        copy_in(&mut tx, "t_out", buf).await?;
        exec(
            &mut tx,
            "INSERT INTO package_outputs (release_id, attr, system, output, path, drv_path)
             SELECT DISTINCT ON (release_id, attr, system, output) release_id, attr, system, output, path, drv_path FROM t_out
             ON CONFLICT DO NOTHING",
        )
        .await?;
    }

    // Programs.
    if let Some(s) = &bundle.programs {
        let rel = with(Dataset::Programs);
        let rel_mask = mask(&rel);
        progress(format!("programs: {} entries", s.entries.len()));
        exec(
            &mut tx,
            "CREATE TEMP TABLE t_prog (release_id int, name text, system text, attr text) ON COMMIT DROP",
        )
        .await?;
        let mut buf = CopyBuf::new();
        for e in &s.entries {
            for id in release_ids(&e.releases, &rel_mask) {
                buf.field(id).field(&e.name).field(&e.system).field(&e.attr);
                buf.end_row();
            }
            report.touched_attrs.insert(e.attr.clone());
        }
        report.programs = buf.rows;
        copy_in(&mut tx, "t_prog", buf).await?;
        exec(
            &mut tx,
            "INSERT INTO programs (release_id, name, system, attr) SELECT release_id, name, system, attr FROM t_prog ON CONFLICT DO NOTHING",
        )
        .await?;
    }

    progress("committing".into());
    tx.commit().await?;
    Ok(report)
}

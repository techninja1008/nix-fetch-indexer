//! Ingesting interchange bundles into Postgres.
//!
//! For every release in the bundle and every dataset the release carries in
//! the bundle, the release's existing rows for that dataset are replaced, so
//! re-ingesting is idempotent.

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
    pub store_paths: usize,
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
        let id: i32 = sqlx::query_scalar(
            "INSERT INTO releases (name, commit, channel, series, url) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (name) DO UPDATE SET commit = EXCLUDED.commit, channel = EXCLUDED.channel,
               series = EXCLUDED.series, url = COALESCE(EXCLUDED.url, releases.url), indexed_at = now()
             RETURNING id",
        )
        .bind(&r.name)
        .bind(&r.commit)
        .bind(&r.channel)
        .bind(&r.series)
        .bind(&r.url)
        .fetch_one(&mut *tx)
        .await
        .with_context(|| format!("upserting release {}", r.name))?;
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
    let release_ids = |bits: &nfi_interchange::Bitset, allowed: &[i32]| -> Vec<i32> {
        bits.iter()
            .map(|i| ids[i])
            .filter(|id| allowed.contains(id))
            .collect()
    };

    // Closure.
    if let Some(c) = &bundle.closure {
        let rel = with(Dataset::Closure);
        progress(format!("closure: {} paths", c.paths.len()));
        sqlx::query("DELETE FROM store_path_releases WHERE release_id = ANY($1)")
            .bind(&rel)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM missing_paths WHERE release_id = ANY($1)")
            .bind(&rel)
            .execute(&mut *tx)
            .await?;
        exec(
            &mut tx,
            "CREATE TEMP TABLE t_paths (idx int, hash text, name text, nar_size bigint, file_size bigint, deriver text) ON COMMIT DROP",
        )
        .await?;
        exec(
            &mut tx,
            "CREATE TEMP TABLE t_refs (idx int, ref int) ON COMMIT DROP",
        )
        .await?;
        exec(
            &mut tx,
            "CREATE TEMP TABLE t_prel (idx int, release_id int) ON COMMIT DROP",
        )
        .await?;
        exec(
            &mut tx,
            "CREATE TEMP TABLE t_missing (hash text, name text, release_id int) ON COMMIT DROP",
        )
        .await?;
        let (mut paths, mut refs, mut prel, mut missing) =
            (CopyBuf::new(), CopyBuf::new(), CopyBuf::new(), CopyBuf::new());
        for (i, p) in c.paths.iter().enumerate() {
            let hash = p.hash.to_string();
            paths
                .field(i as i64)
                .field(&hash)
                .field(&p.name)
                .field(p.nar_size)
                .field(p.file_size)
                .field(&p.deriver);
            paths.end_row();
            for &r in &p.references {
                refs.field(i as i64).field(r);
                refs.end_row();
            }
            for id in release_ids(&p.releases, &rel) {
                prel.field(i as i64).field(id);
                prel.end_row();
            }
        }
        for m in &c.missing {
            let hash = m.hash.to_string();
            for id in release_ids(&m.releases, &rel) {
                missing.field(&hash).field(&m.name).field(id);
                missing.end_row();
            }
        }
        report.store_paths = paths.rows;
        report.references = refs.rows;
        report.missing = missing.rows;
        copy_in(&mut tx, "t_paths", paths).await?;
        copy_in(&mut tx, "t_refs", refs).await?;
        copy_in(&mut tx, "t_prel", prel).await?;
        copy_in(&mut tx, "t_missing", missing).await?;
        exec(&mut tx, "ANALYZE t_paths").await?;
        progress("closure: inserting paths".into());
        exec(
            &mut tx,
            "INSERT INTO store_paths (hash, name, nar_size, file_size, deriver)
             SELECT hash, name, nar_size, file_size, deriver FROM t_paths
             ON CONFLICT (hash) DO UPDATE SET
               nar_size = COALESCE(store_paths.nar_size, EXCLUDED.nar_size),
               file_size = COALESCE(store_paths.file_size, EXCLUDED.file_size),
               deriver = COALESCE(store_paths.deriver, EXCLUDED.deriver)
             WHERE (store_paths.nar_size IS NULL AND EXCLUDED.nar_size IS NOT NULL)
                OR (store_paths.file_size IS NULL AND EXCLUDED.file_size IS NOT NULL)
                OR (store_paths.deriver IS NULL AND EXCLUDED.deriver IS NOT NULL)",
        )
        .await?;
        exec(
            &mut tx,
            "CREATE TEMP TABLE t_map ON COMMIT DROP AS SELECT t.idx, s.id FROM t_paths t JOIN store_paths s ON s.hash = t.hash",
        )
        .await?;
        exec(&mut tx, "CREATE INDEX ON t_map (idx)").await?;
        exec(&mut tx, "ANALYZE t_map").await?;
        exec(&mut tx, "ANALYZE t_refs").await?;
        exec(&mut tx, "ANALYZE t_prel").await?;
        progress("closure: inserting references".into());
        exec(
            &mut tx,
            "INSERT INTO store_path_refs (path_id, ref_id)
             SELECT a.id, b.id FROM t_refs r JOIN t_map a ON a.idx = r.idx JOIN t_map b ON b.idx = r.ref
             ON CONFLICT DO NOTHING",
        )
        .await?;
        progress("closure: inserting release membership".into());
        exec(
            &mut tx,
            "INSERT INTO store_path_releases (path_id, release_id)
             SELECT m.id, t.release_id FROM t_prel t JOIN t_map m ON m.idx = t.idx ON CONFLICT DO NOTHING",
        )
        .await?;
        exec(
            &mut tx,
            "INSERT INTO missing_paths (hash, name, release_id) SELECT hash, name, release_id FROM t_missing ON CONFLICT DO NOTHING",
        )
        .await?;
    }

    // Packages.
    if let Some(s) = &bundle.packages {
        let rel = with(Dataset::Packages);
        progress(format!("packages: {} entries", s.entries.len()));
        let old: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT p.attr FROM packages p JOIN package_releases pr ON pr.package_id = p.id WHERE pr.release_id = ANY($1)",
        )
        .bind(&rel)
        .fetch_all(&mut *tx)
        .await?;
        report.touched_attrs.extend(old);
        sqlx::query("DELETE FROM package_releases WHERE release_id = ANY($1)")
            .bind(&rel)
            .execute(&mut *tx)
            .await?;
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
            for id in release_ids(&p.releases, &rel) {
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
        progress(format!("outpaths: {} entries", s.entries.len()));
        sqlx::query("DELETE FROM package_outputs WHERE release_id = ANY($1)")
            .bind(&rel)
            .execute(&mut *tx)
            .await?;
        exec(
            &mut tx,
            "CREATE TEMP TABLE t_out (release_id int, attr text, system text, output text, path text, drv_path text) ON COMMIT DROP",
        )
        .await?;
        let mut buf = CopyBuf::new();
        for e in &s.entries {
            for id in release_ids(&e.releases, &rel) {
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
             ON CONFLICT (release_id, attr, system, output) DO UPDATE SET path = EXCLUDED.path, drv_path = EXCLUDED.drv_path",
        )
        .await?;
    }

    // Programs.
    if let Some(s) = &bundle.programs {
        let rel = with(Dataset::Programs);
        progress(format!("programs: {} entries", s.entries.len()));
        let old: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT attr FROM programs WHERE release_id = ANY($1)")
                .bind(&rel)
                .fetch_all(&mut *tx)
                .await?;
        report.touched_attrs.extend(old);
        sqlx::query("DELETE FROM programs WHERE release_id = ANY($1)")
            .bind(&rel)
            .execute(&mut *tx)
            .await?;
        exec(
            &mut tx,
            "CREATE TEMP TABLE t_prog (release_id int, name text, system text, attr text) ON COMMIT DROP",
        )
        .await?;
        let mut buf = CopyBuf::new();
        for e in &s.entries {
            for id in release_ids(&e.releases, &rel) {
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
    crate::prune::gc(&mut tx).await?;
    tx.commit().await?;
    Ok(report)
}

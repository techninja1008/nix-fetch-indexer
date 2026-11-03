//! Meilisearch integration: document building, sync and queries.

use anyhow::{Context, Result, bail};
use nfi_api::{SearchHit, SearchQuery, SearchResponse};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// Advisory lock key serialising all search index writes ([`Meili::sync_attrs`]
/// and [`Meili::reindex`]), across replicas.
///
/// Distinct from [`crate::ingest::WRITE_LOCK`]: a rebuild can take minutes,
/// and ingest and prune need not wait for it. Search writers only read
/// committed index data after taking this lock, and every ingest or prune
/// syncs search after committing, so the newest sync runs last: a write
/// committed after a rebuild read the database has its sync queued behind the
/// rebuild, and a rebuild never swaps in documents older than a sync that ran
/// before it.
pub const SEARCH_LOCK: i64 = 0x6e66_6901;

/// How long to wait for one Meilisearch task (index creation, a document
/// batch, a swap) before giving up.
const TASK_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone)]
pub struct Meili {
    client: reqwest::Client,
    url: String,
    key: Option<String>,
    pub index: String,
}

pub fn doc_id(attr: &str, version: Option<&str>, system: &str) -> String {
    let mut h = Sha256::new();
    h.update(attr.as_bytes());
    h.update([0]);
    h.update(version.unwrap_or("").as_bytes());
    h.update([0]);
    h.update(system.as_bytes());
    hex::encode(&h.finalize()[..16])
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

impl Meili {
    pub fn new(url: &str, key: Option<String>, index: &str) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()?,
            url: url.trim_end_matches('/').to_string(),
            key,
            index: index.to_string(),
        })
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let r = self.client.request(method, format!("{}{}", self.url, path));
        match &self.key {
            Some(k) => r.bearer_auth(k),
            None => r,
        }
    }

    async fn send(&self, rb: reqwest::RequestBuilder) -> Result<Value> {
        let resp = rb.send().await.context("meilisearch request")?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("meilisearch: HTTP {status}: {body}");
        }
        Ok(body)
    }

    /// Wait for an enqueued task to finish.
    async fn wait(&self, task: &Value) -> Result<()> {
        let Some(uid) = task.get("taskUid").and_then(Value::as_u64) else {
            return Ok(());
        };
        let deadline = tokio::time::Instant::now() + TASK_TIMEOUT;
        let mut delay = Duration::from_millis(20);
        loop {
            let t = self
                .send(self.req(reqwest::Method::GET, &format!("/tasks/{uid}")))
                .await?;
            match t.get("status").and_then(Value::as_str) {
                Some("succeeded") => return Ok(()),
                Some("failed") | Some("canceled") => bail!("meilisearch task {uid} failed: {}", t["error"]),
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "meilisearch task {uid} did not finish within {}s (status {})",
                    TASK_TIMEOUT.as_secs(),
                    t["status"]
                );
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_millis(500));
        }
    }

    async fn index_exists(&self, uid: &str) -> Result<bool> {
        Ok(self
            .req(reqwest::Method::GET, &format!("/indexes/{uid}"))
            .send()
            .await?
            .status()
            .is_success())
    }

    async fn ensure_index_named(&self, uid: &str) -> Result<()> {
        if !self.index_exists(uid).await? {
            let t = self
                .send(
                    self.req(reqwest::Method::POST, "/indexes")
                        .json(&json!({"uid": uid, "primaryKey": "id"})),
                )
                .await?;
            self.wait(&t).await?;
        }
        let t = self
            .send(self.req(reqwest::Method::PATCH, &format!("/indexes/{uid}/settings")).json(&json!({
                "searchableAttributes": ["attr", "pname", "programs", "main_program", "description"],
                "filterableAttributes": ["attr", "pname", "version", "system", "programs", "releases", "channels", "series", "commits"],
                "pagination": {"maxTotalHits": 10000}
            })))
            .await?;
        self.wait(&t).await
    }

    pub async fn ensure_index(&self) -> Result<()> {
        self.ensure_index_named(&self.index).await
    }

    async fn add_documents(&self, uid: &str, docs: &[SearchHit]) -> Result<()> {
        for chunk in docs.chunks(10_000) {
            let t = self
                .send(
                    self.req(
                        reqwest::Method::POST,
                        &format!("/indexes/{uid}/documents?primaryKey=id"),
                    )
                    .json(chunk),
                )
                .await?;
            self.wait(&t).await?;
        }
        Ok(())
    }

    async fn delete_documents(&self, uid: &str, ids: &[String]) -> Result<()> {
        for chunk in ids.chunks(10_000) {
            let t = self
                .send(
                    self.req(
                        reqwest::Method::POST,
                        &format!("/indexes/{uid}/documents/delete-batch"),
                    )
                    .json(chunk),
                )
                .await?;
            self.wait(&t).await?;
        }
        Ok(())
    }

    async fn doc_ids_for_attrs(&self, attrs: &[String]) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for chunk in attrs.chunks(200) {
            let filter = format!(
                "attr IN [{}]",
                chunk.iter().map(|a| quote(a)).collect::<Vec<_>>().join(", ")
            );
            let mut offset = 0;
            loop {
                let r = self
                    .send(
                        self.req(
                            reqwest::Method::POST,
                            &format!("/indexes/{}/documents/fetch", self.index),
                        )
                        .json(&json!({"filter": filter, "fields": ["id"], "limit": 1000, "offset": offset})),
                    )
                    .await?;
                let results = r["results"].as_array().map_or(&[][..], Vec::as_slice);
                ids.extend(
                    results
                        .iter()
                        .filter_map(|d| d["id"].as_str().map(str::to_string)),
                );
                if results.len() < 1000 {
                    break;
                }
                offset += 1000;
            }
        }
        Ok(ids)
    }

    /// Take [`SEARCH_LOCK`], held until the returned transaction ends
    /// (also if the caller is cancelled: the transaction is rolled back).
    async fn lock(pool: &PgPool) -> Result<sqlx::Transaction<'static, sqlx::Postgres>> {
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(SEARCH_LOCK)
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }

    async fn delete_index(&self, uid: &str) -> Result<()> {
        let t = self
            .send(self.req(reqwest::Method::DELETE, &format!("/indexes/{uid}")))
            .await?;
        self.wait(&t).await
    }

    /// Whether `uid` is a scratch index of a rebuild of this index.
    fn is_scratch(&self, uid: &str) -> bool {
        uid.strip_prefix(&self.index)
            .and_then(|s| s.strip_prefix("_rebuild"))
            .is_some_and(|s| {
                // Unique scratch names, or the fixed one older versions used.
                s.is_empty()
                    || s.strip_prefix('_')
                        .is_some_and(|h| h.len() == 32 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            })
    }

    /// Scratch indexes left behind by interrupted rebuilds.
    async fn leftover_scratch(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut offset = 0;
        loop {
            let r = self
                .send(self.req(
                    reqwest::Method::GET,
                    &format!("/indexes?limit=1000&offset={offset}"),
                ))
                .await?;
            let results = r["results"].as_array().map_or(&[][..], Vec::as_slice);
            out.extend(
                results
                    .iter()
                    .filter_map(|i| i["uid"].as_str())
                    .filter(|uid| self.is_scratch(uid))
                    .map(str::to_string),
            );
            if results.len() < 1000 {
                return Ok(out);
            }
            offset += 1000;
        }
    }

    /// Refresh the documents of the given attributes from Postgres.
    pub async fn sync_attrs(&self, pool: &PgPool, attrs: &BTreeSet<String>) -> Result<usize> {
        if attrs.is_empty() {
            return Ok(0);
        }
        let lock = Self::lock(pool).await?;
        self.ensure_index().await?;
        let attrs: Vec<String> = attrs.iter().cloned().collect();
        let mut total = 0;
        for chunk in attrs.chunks(20_000) {
            let docs = build_docs(pool, Some(chunk)).await?;
            let keep: BTreeSet<&str> = docs.iter().map(|d| d.id.as_str()).collect();
            let stale: Vec<String> = self
                .doc_ids_for_attrs(chunk)
                .await?
                .into_iter()
                .filter(|id| !keep.contains(id.as_str()))
                .collect();
            self.add_documents(&self.index, &docs).await?;
            self.delete_documents(&self.index, &stale).await?;
            total += docs.len();
        }
        lock.commit().await?;
        Ok(total)
    }

    /// Rebuild the whole index into a fresh one and swap it in.
    pub async fn reindex(&self, pool: &PgPool) -> Result<usize> {
        let mut lock = Self::lock(pool).await?;
        self.ensure_index().await?;
        // Rebuilds are serialised, so any scratch index is a leftover from an
        // interrupted one.
        for uid in self.leftover_scratch().await? {
            tracing::info!(index = %uid, "deleting leftover search rebuild index");
            self.delete_index(&uid).await?;
        }
        // A scratch index of its own, so even a rebuild that outlived its
        // lock (its database connection was lost) can't clobber another's.
        let suffix: String = sqlx::query_scalar("SELECT replace(gen_random_uuid()::text, '-', '')")
            .fetch_one(&mut *lock)
            .await?;
        let tmp = format!("{}_rebuild_{suffix}", self.index);
        let res = async {
            self.ensure_index_named(&tmp).await?;
            let docs = build_docs(pool, None).await?;
            self.add_documents(&tmp, &docs).await?;
            let t = self
                .send(
                    self.req(reqwest::Method::POST, "/swap-indexes")
                        .json(&json!([{"indexes": [self.index, tmp]}])),
                )
                .await?;
            self.wait(&t).await?;
            Ok(docs.len())
        }
        .await;
        // After a swap, the scratch index holds the old documents; after a
        // failure, whatever was built.
        let cleanup = async {
            if self.index_exists(&tmp).await? {
                self.delete_index(&tmp).await?;
            }
            anyhow::Ok(())
        };
        if let Err(e) = cleanup.await {
            tracing::warn!(index = %tmp, error = %format!("{e:#}"), "deleting search rebuild index");
        }
        lock.commit().await?;
        res
    }

    pub async fn search(&self, q: &SearchQuery) -> Result<SearchResponse> {
        let mut filter = Vec::new();
        let mut eq = |field: &str, v: &Option<String>| {
            if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
                filter.push(format!("{field} = {}", quote(v)));
            }
        };
        eq("attr", &q.attr);
        eq("pname", &q.pname);
        eq("version", &q.version);
        eq("programs", &q.program);
        eq("system", &q.system);
        eq("releases", &q.release);
        eq("channels", &q.channel);
        eq("series", &q.series);
        eq("commits", &q.commit);
        let limit = q.limit.unwrap_or(20).min(1000);
        let offset = q.offset.unwrap_or(0);
        let r = self
            .send(
                self.req(reqwest::Method::POST, &format!("/indexes/{}/search", self.index))
                    .json(&json!({
                        "q": q.q.clone().unwrap_or_default(),
                        "filter": filter,
                        "limit": limit,
                        "offset": offset,
                    })),
            )
            .await?;
        let hits: Vec<SearchHit> =
            serde_json::from_value(r["hits"].clone()).context("decoding search hits")?;
        Ok(SearchResponse {
            hits,
            estimated_total: r["estimatedTotalHits"].as_u64().unwrap_or(0) as usize,
            offset,
            limit,
        })
    }
}

type PkgRow = (
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
);

/// Build search documents (for all attributes, or the given ones).
pub async fn build_docs(pool: &PgPool, attrs: Option<&[String]>) -> Result<Vec<SearchHit>> {
    let attrs: Option<Vec<String>> = attrs.map(|a| a.to_vec());
    let rows: Vec<PkgRow> = sqlx::query_as(
        "SELECT p.attr, p.version, p.system,
                (array_agg(p.pname ORDER BY r.id DESC))[1],
                (array_agg(p.description ORDER BY r.id DESC))[1],
                (array_agg(p.main_program ORDER BY r.id DESC))[1],
                array_agg(DISTINCT r.name), array_agg(DISTINCT r.channel),
                array_agg(DISTINCT r.series), array_agg(DISTINCT r.commit)
         FROM packages p
         JOIN package_releases pr ON pr.package_id = p.id
         JOIN releases r ON r.id = pr.release_id
         WHERE $1::text[] IS NULL OR p.attr = ANY($1)
         GROUP BY p.attr, p.version, p.system",
    )
    .bind(&attrs)
    .fetch_all(pool)
    .await?;
    let progs: Vec<(String, String, String, Vec<String>)> = sqlx::query_as(
        "SELECT pg.attr, pg.system, r.name, array_agg(DISTINCT pg.name)
         FROM programs pg JOIN releases r ON r.id = pg.release_id
         WHERE $1::text[] IS NULL OR pg.attr = ANY($1)
         GROUP BY pg.attr, pg.system, r.name",
    )
    .bind(&attrs)
    .fetch_all(pool)
    .await?;
    // (attr, system) -> release -> program names
    let mut prog_map: BTreeMap<(String, String), BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for (attr, system, rel, names) in progs {
        prog_map.entry((attr, system)).or_default().insert(rel, names);
    }
    let releases: Vec<(String, String, String, String)> =
        sqlx::query_as("SELECT name, channel, series, commit FROM releases")
            .fetch_all(pool)
            .await?;
    let release_meta: BTreeMap<String, (String, String, String)> = releases
        .into_iter()
        .map(|(n, c, s, co)| (n, (c, s, co)))
        .collect();

    let mut docs = Vec::with_capacity(rows.len());
    let mut covered: BTreeSet<(String, String, String)> = BTreeSet::new();
    for (attr, version, system, pname, description, main_program, rels, channels, series, commits) in rows {
        let mut programs = BTreeSet::new();
        if let Some(by_rel) = prog_map.get(&(attr.clone(), system.clone())) {
            for r in &rels {
                if let Some(names) = by_rel.get(r) {
                    programs.extend(names.iter().cloned());
                    covered.insert((attr.clone(), system.clone(), r.clone()));
                }
            }
        }
        docs.push(SearchHit {
            id: doc_id(&attr, version.as_deref(), &system),
            attr,
            pname,
            version,
            system,
            description,
            main_program,
            programs: programs.into_iter().collect(),
            releases: rels,
            channels,
            series,
            commits,
        });
    }
    // Programs whose attribute has no package metadata in that release.
    for ((attr, system), by_rel) in prog_map {
        let rels: Vec<&String> = by_rel
            .keys()
            .filter(|r| !covered.contains(&(attr.clone(), system.clone(), (*r).clone())))
            .collect();
        if rels.is_empty() {
            continue;
        }
        let mut programs = BTreeSet::new();
        let (mut channels, mut series, mut commits) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        for r in &rels {
            programs.extend(by_rel[*r].iter().cloned());
            if let Some((c, s, co)) = release_meta.get(*r) {
                channels.insert(c.clone());
                series.insert(s.clone());
                commits.insert(co.clone());
            }
        }
        docs.push(SearchHit {
            id: doc_id(&attr, None, &system),
            attr: attr.clone(),
            pname: None,
            version: None,
            system,
            description: None,
            main_program: None,
            programs: programs.into_iter().collect(),
            releases: rels.into_iter().cloned().collect(),
            channels: channels.into_iter().collect(),
            series: series.into_iter().collect(),
            commits: commits.into_iter().collect(),
        });
    }
    // A versionless programs doc may collide with a versionless package doc.
    let mut seen = BTreeMap::new();
    let mut out: Vec<SearchHit> = Vec::with_capacity(docs.len());
    for d in docs {
        match seen.get(&d.id) {
            Some(&i) => {
                let e: &mut SearchHit = &mut out[i];
                let merge = |a: &mut Vec<String>, b: Vec<String>| {
                    let s: BTreeSet<String> = a.drain(..).chain(b).collect();
                    *a = s.into_iter().collect();
                };
                merge(&mut e.programs, d.programs);
                merge(&mut e.releases, d.releases);
                merge(&mut e.channels, d.channels);
                merge(&mut e.series, d.series);
                merge(&mut e.commits, d.commits);
            }
            None => {
                seen.insert(d.id.clone(), out.len());
                out.push(d);
            }
        }
    }
    Ok(out)
}

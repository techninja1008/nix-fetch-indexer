//! Request/response types shared by the query service and CLIs, plus the
//! derivation-graph reduction and minimise planning algorithms.

#[cfg(feature = "client")]
pub mod client;
pub mod drv;
pub mod minimise;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const DEFAULT_SYSTEM: &str = "x86_64-linux";

// ---------- search ----------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SearchQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
}

/// A search document: one per (attr, version, system).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub id: String,
    pub attr: String,
    #[serde(default)]
    pub pname: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    pub system: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub main_program: Option<String>,
    #[serde(default)]
    pub programs: Vec<String>,
    #[serde(default)]
    pub releases: Vec<String>,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub series: Vec<String>,
    #[serde(default)]
    pub commits: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub estimated_total: usize,
    pub offset: usize,
    pub limit: usize,
}

// ---------- lookups ----------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRef {
    pub name: String,
    pub commit: String,
    pub channel: String,
    pub series: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub path: String,
    /// Whether the path is in the closure index (i.e. available in the cache).
    pub cached: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackageInfo {
    pub attr: String,
    pub system: String,
    pub release: ReleaseRef,
    pub name: Option<String>,
    pub pname: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    #[serde(default)]
    pub license: Vec<String>,
    pub main_program: Option<String>,
    #[serde(default)]
    pub broken: bool,
    #[serde(default)]
    pub unfree: bool,
    pub drv_path: Option<String>,
    /// Output name to path; empty when no out-path data is indexed.
    pub outputs: BTreeMap<String, OutputInfo>,
    #[serde(default)]
    pub programs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkItem {
    pub attr: String,
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Outputs to include; default `["out"]` (or all, if there is no `out`).
    /// `["*"]` selects all outputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BulkRequest {
    pub items: Vec<BulkItem>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Ok,
    UnknownCommit,
    AmbiguousCommit,
    UnknownAttr,
    /// The attribute exists but no evaluated out paths are indexed for it.
    NoOutpaths,
    /// Out paths are known but (some of) them are not in the cache.
    NotCached,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BulkItemResult {
    pub attr: String,
    pub commit: String,
    pub system: String,
    pub status: ItemStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// Selected outputs to full store paths.
    pub outputs: BTreeMap<String, OutputInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BulkResponse {
    pub items: Vec<BulkItemResult>,
    /// Deduplicated runtime closure of all cached selected outputs.
    pub closure: Vec<String>,
    pub nar_size: u64,
    pub file_size: u64,
}

// ---------- minimise ----------

/// Result of a minimise query.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MinimiseResponse {
    /// Derivations not referenced by any other derivation in the input.
    pub requested: Vec<String>,
    /// Derivations that are not cached and must be built locally.
    pub to_build: Vec<String>,
    /// Fixed-output derivations (fetches) that are not cached and must be
    /// downloaded from upstream.
    #[serde(default)]
    pub to_fetch: Vec<String>,
    /// Cached paths directly needed (not inside another root's closure).
    pub roots: Vec<String>,
    /// Deduplicated runtime closure of all directly needed cached paths.
    pub closure: Vec<String>,
    /// Input derivations referenced but absent from the submitted graph.
    #[serde(default)]
    pub unresolved: Vec<String>,
    pub nar_size: u64,
    pub file_size: u64,
    /// Problems the client should surface to the user.
    #[serde(default)]
    pub warnings: Vec<Warning>,
}

/// A machine-readable warning with a human-readable message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    /// Stable identifier, e.g. `unindexed_nixpkgs`.
    pub code: String,
    pub message: String,
    /// Store or derivation paths the warning is about.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

// ---------- admin ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub name: String,
    pub commit: String,
    pub channel: String,
    pub series: String,
    pub url: Option<String>,
    pub datasets: Vec<String>,
    /// The release's closures, by type.
    #[serde(default)]
    pub closures: Vec<ClosureInfo>,
    /// The release's evaluations, by type.
    #[serde(default)]
    pub evals: Vec<EvalInfo>,
    pub indexed_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClosureInfo {
    pub closure_type: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvalInfo {
    pub eval_type: String,
    /// Attributes that failed to evaluate, as `<attr>@<system>`.
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IngestUrlRequest {
    pub url: String,
    /// Closure type to read a version 2 bundle (which predates closure
    /// types) as; default `store-paths`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v2_closure_type: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PruneRequest {
    #[serde(default)]
    pub releases: Vec<String>,
    #[serde(default)]
    pub commits: Vec<String>,
    #[serde(default)]
    pub channels: Vec<String>,
    /// Only remove closures of these types from the selected releases
    /// (the releases themselves and their other data are kept). Empty
    /// removes the selected releases entirely.
    #[serde(default)]
    pub closure_types: Vec<String>,
}

impl PruneRequest {
    pub fn is_empty(&self) -> bool {
        self.releases.is_empty() && self.commits.is_empty() && self.channels.is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobCreated {
    pub id: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
}

impl JobStatus {
    pub fn is_finished(&self) -> bool {
        matches!(self, JobStatus::Succeeded | JobStatus::Failed)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: i64,
    pub kind: String,
    pub status: JobStatus,
    pub params: serde_json::Value,
    pub progress: Option<String>,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// One kind of index inconsistency found by a consistency check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckIssue {
    /// `dangling_refs`, `unknown_closures`, `orphan_paths` or `open_closures`.
    pub kind: String,
    pub description: String,
    pub count: u64,
    /// A few affected rows, for diagnosis.
    #[serde(default)]
    pub examples: Vec<String>,
}

/// Result of a consistency check of the closure index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckReport {
    pub ok: bool,
    pub store_paths: u64,
    pub references: u64,
    pub releases: u64,
    #[serde(default)]
    pub closures: u64,
    #[serde(default)]
    pub evals: u64,
    #[serde(default)]
    pub eval_errors: u64,
    /// Only kinds with at least one occurrence are listed.
    pub issues: Vec<CheckIssue>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

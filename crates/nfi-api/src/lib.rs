//! Request/response types shared by the query service and CLIs.

pub mod drv;

pub use nfi_auth::ErrorBody;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const DEFAULT_SYSTEM: &str = "x86_64-linux";

// ---------- routes ----------

/// GET, [`SearchQuery`] -> [`SearchResponse`].
pub const SEARCH_PATH: &str = "/v1/search";
/// GET `{PACKAGES_PATH}/<attr>`, [`PackageParams`] -> [`PackageInfo`].
pub const PACKAGES_PATH: &str = "/v1/packages";
/// POST [`BulkRequest`] -> [`BulkResponse`].
pub const BULK_PATH: &str = "/v1/lookup/bulk";
/// Most items a [`BulkRequest`] may hold; larger requests are refused (400)
/// and should be split.
pub const BULK_MAX_ITEMS: usize = 10_000;
/// POST a derivation graph, [`MinimiseParams`] -> [`MinimiseResponse`].
pub const MINIMISE_PATH: &str = "/v1/minimise";
/// GET, [`ReleasesParams`] -> `Vec<`[`ReleaseInfo`]`>`.
pub const RELEASES_PATH: &str = "/v1/releases";
/// POST a bundle ([`IngestParams`]) or an [`IngestUrlRequest`] ->
/// [`JobCreated`]; the job's result is an [`IngestReport`].
pub const ADMIN_INGEST_PATH: &str = "/admin/v1/ingest";
/// GET, [`ListParams`] -> `Vec<`[`JobInfo`]`>`; GET `{ADMIN_JOBS_PATH}/<id>`
/// -> [`JobInfo`].
pub const ADMIN_JOBS_PATH: &str = "/admin/v1/jobs";
/// [`RELEASES_PATH`] on the admin listener.
pub const ADMIN_RELEASES_PATH: &str = "/admin/v1/releases";
/// POST [`PruneRequest`] -> [`JobCreated`]; the job's result is a
/// [`PruneReport`].
pub const ADMIN_PRUNE_PATH: &str = "/admin/v1/prune";
/// POST -> [`JobCreated`]; the job's result is `{"search_documents": n}`.
pub const ADMIN_REINDEX_PATH: &str = "/admin/v1/reindex";
/// POST -> [`JobCreated`]; the job's result is a [`CheckReport`].
pub const ADMIN_CHECK_PATH: &str = "/admin/v1/check";

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
    /// A commit hash or a prefix of at least 8 hex digits, in either case,
    /// selecting one indexed release (400 if malformed or ambiguous, 404 if
    /// unknown).
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

/// Query parameters of [`PACKAGES_PATH`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackageParams {
    pub commit: String,
    /// Default [`DEFAULT_SYSTEM`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
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
    UnknownAttr,
    /// The attribute exists but no evaluated out paths are indexed for it.
    NoOutpaths,
    /// Out paths are known but (some of) them are not in the cache.
    NotCached,
    /// Some requested outputs don't exist (see `missing_outputs`).
    UnknownOutput,
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
    /// With `unknown_output`: the requested outputs that don't exist, and
    /// the outputs that do.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_outputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_outputs: Vec<String>,
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

/// Query parameters of [`MINIMISE_PATH`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MinimiseParams {
    /// A derivation name matching this many indexed paths or more is too
    /// common for a [`CLOSE_MISS`] warning; server default when `None`, 0
    /// disables the check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_miss_threshold: Option<u64>,
}

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

/// [`Warning::code`]: the graph was most likely evaluated from a nixpkgs
/// revision that is not indexed.
pub const UNINDEXED_NIXPKGS: &str = "unindexed_nixpkgs";
/// [`Warning::code`]: derivations to build whose name matches indexed paths.
pub const CLOSE_MISS: &str = "close_miss";

// ---------- releases ----------

/// Query parameters of [`RELEASES_PATH`] and [`ADMIN_RELEASES_PATH`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReleasesParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// List each evaluation's errors, not only their count.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub errors: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub name: String,
    pub commit: String,
    pub channel: String,
    pub series: String,
    pub url: Option<String>,
    pub datasets: Vec<String>,
    /// The release's evaluations, by eval type, with their closures.
    #[serde(default)]
    pub evals: Vec<EvalInfo>,
    pub indexed_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvalInfo {
    pub eval_type: String,
    /// Closure type names (within the eval type; may be empty) of the
    /// evaluation's closures.
    #[serde(default)]
    pub closures: Vec<String>,
    /// How many attributes failed to evaluate; `None` when the evaluation's
    /// errors are not indexed.
    #[serde(default)]
    pub error_count: Option<u64>,
    /// Attributes that failed to evaluate, as `<attr>@<system>`; only listed
    /// when requested (`errors=true`), and `None` when the evaluation's errors
    /// are not indexed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<String>>,
}

// ---------- admin ----------

/// Query parameters of an upload to [`ADMIN_INGEST_PATH`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IngestParams {
    /// Closure type to read a version 2 bundle (which predates closure
    /// types) as; default `store-paths`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v2_closure_type: Option<String>,
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
    /// Only remove the closures of these closure types (`<eval
    /// type>[:<closure type>]`, e.g. `helpers:full`) from the selected
    /// releases. The releases and their other data are kept; evaluations
    /// left with neither closures nor indexed errors are removed. Empty
    /// removes the selected releases entirely.
    #[serde(default)]
    pub closure_types: Vec<String>,
}

impl PruneRequest {
    pub fn is_empty(&self) -> bool {
        self.releases.is_empty() && self.commits.is_empty() && self.channels.is_empty()
    }
}

/// Query parameters of [`ADMIN_JOBS_PATH`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ListParams {
    /// Default 50, at most 1000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
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

/// Result of an ingest job.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
    /// `<release> <eval type>` for each evaluation ingested.
    pub evals: Vec<String>,
    pub eval_errors: usize,
    /// Search documents updated; absent without search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_documents: Option<usize>,
}

/// Result of a prune job.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PruneReport {
    /// Releases removed entirely.
    pub releases: Vec<String>,
    /// `<release> <closure type>` for closures removed from kept releases.
    pub closures: Vec<String>,
    pub store_paths_removed: u64,
    pub packages_removed: u64,
    /// Search documents after rebuilding the search index; absent without
    /// search or when no release was removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_documents: Option<usize>,
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

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

// ---------- releases ----------

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

impl EvalInfo {
    /// The error count, falling back to the listed errors (services that
    /// predate `error_count`).
    pub fn error_count(&self) -> Option<u64> {
        self.error_count
            .or_else(|| self.errors.as_ref().map(|e| e.len() as u64))
    }
}

/// A human-readable table of releases, one per line after a header; listed
/// errors follow their release, one evaluation per line.
pub fn releases_table(rels: &[ReleaseInfo]) -> String {
    let mut out = format!(
        "{:<40} {:<18} {:<12} {:<42} {:<28} EVALS\n",
        "RELEASE", "CHANNEL", "SERIES", "COMMIT", "DATASETS"
    );
    for r in rels {
        // e.g. `helpers[direct,full] (2 errors)`, `store-paths`.
        let evals: Vec<String> = r
            .evals
            .iter()
            .map(|e| {
                let mut s = e.eval_type.clone();
                if e.closures.iter().any(|c| !c.is_empty()) {
                    s += &format!("[{}]", e.closures.join(","));
                }
                if let Some(n) = e.error_count() {
                    s += &format!(" ({n} errors)");
                }
                s
            })
            .collect();
        out += &format!(
            "{:<40} {:<18} {:<12} {:<42} {:<28} {}\n",
            r.name,
            r.channel,
            r.series,
            r.commit,
            r.datasets.join(","),
            evals.join(", ")
        );
        for e in &r.evals {
            if let Some(errors) = e.errors.as_ref().filter(|e| !e.is_empty()) {
                out += &format!("    {} errors: {}\n", e.eval_type, errors.join(" "));
            }
        }
    }
    out
}

// ---------- admin ----------

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

    /// The first commit that is not a valid commit prefix, if any. Commits
    /// select releases by prefix, so an empty one would select them all.
    pub fn invalid_commit(&self) -> Option<&str> {
        self.commits
            .iter()
            .map(String::as_str)
            .find(|c| !is_commit_prefix(c))
    }
}

/// Whether `s` is a non-empty string of hex digits (either case), usable as
/// a commit prefix.
pub fn is_commit_prefix(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releases_table_counts_and_lists_errors() {
        let eval = |errors: Option<Vec<&str>>| EvalInfo {
            eval_type: "helpers".into(),
            closures: vec!["direct".into()],
            error_count: Some(2),
            errors: errors.map(|e| e.into_iter().map(String::from).collect()),
        };
        let mut r = ReleaseInfo {
            name: "nixos-25.05.1.aaaa".into(),
            commit: "a".repeat(40),
            channel: "nixos-25.05".into(),
            series: "25.05".into(),
            url: None,
            datasets: vec!["closure".into()],
            evals: vec![eval(None)],
            indexed_at: chrono::DateTime::UNIX_EPOCH,
        };
        let t = releases_table(std::slice::from_ref(&r));
        assert_eq!(t.lines().count(), 2, "{t}");
        assert!(t.contains("helpers[direct] (2 errors)"), "{t}");
        r.evals = vec![eval(Some(vec!["a@x86_64-linux", "b@aarch64-linux"]))];
        let t = releases_table(&[r]);
        assert!(
            t.ends_with("    helpers errors: a@x86_64-linux b@aarch64-linux\n"),
            "{t}"
        );
    }

    #[test]
    fn prune_request_rejects_non_hex_commits() {
        let req = |c: &str| PruneRequest {
            commits: vec!["abc123".into(), c.into()],
            ..Default::default()
        };
        assert_eq!(req("DEADbeef").invalid_commit(), None);
        for bad in ["", "%", "ab_c", "abc%", " abc", "xyz"] {
            assert_eq!(req(bad).invalid_commit(), Some(bad));
        }
    }
}

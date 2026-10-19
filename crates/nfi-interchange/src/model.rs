//! Bundle data model.

use crate::bitset::Bitset;
use crate::store_path::StoreHash;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const FORMAT_VERSION: u32 = 3;

/// The closure type of the runtime closures of a release's `store-paths.xz`,
/// and the usual type to read version 2 bundles (which predate closure
/// types) as.
pub const STORE_PATHS: &str = "store-paths";

/// Entry types serialise as named-field objects in human-readable formats
/// (JSON) and as positional arrays in the binary form, which keeps field
/// names out of every entry. The derived (map) impls are generated as
/// inherent functions via `#[serde(remote = "Self")]`.
macro_rules! compact_binary {
    ($ty:ident { $($f:ident),* $(,)? }) => {
        impl Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                if s.is_human_readable() {
                    $ty::serialize(self, s)
                } else {
                    ($(&self.$f,)*).serialize(s)
                }
            }
        }
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                if d.is_human_readable() {
                    $ty::deserialize(d)
                } else {
                    let ($($f,)*) = Deserialize::deserialize(d)?;
                    Ok($ty { $($f),* })
                }
            }
        }
    };
}

/// The kinds of dataset a bundle section can hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Dataset {
    Closure,
    Packages,
    Outpaths,
    Programs,
    EvalErrors,
}

impl Dataset {
    pub const ALL: [Dataset; 5] = [
        Dataset::Closure,
        Dataset::Packages,
        Dataset::Outpaths,
        Dataset::Programs,
        Dataset::EvalErrors,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Dataset::Closure => "closure",
            Dataset::Packages => "packages",
            Dataset::Outpaths => "outpaths",
            Dataset::Programs => "programs",
            Dataset::EvalErrors => "eval-errors",
        }
    }
}

impl std::str::FromStr for Dataset {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Dataset::ALL.into_iter().find(|d| d.as_str() == s).ok_or_else(|| {
            format!("unknown dataset {s:?} (expected closure, packages, outpaths, programs or eval-errors)")
        })
    }
}

impl std::fmt::Display for Dataset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A NixOS / nixpkgs release, identified by its release name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// e.g. `nixos-25.05.813814.ac62194c3917`
    pub name: String,
    /// Full 40 character nixpkgs commit hash.
    pub commit: String,
    /// e.g. `nixos-25.05`, `nixos-unstable`, `nixpkgs-unstable`
    pub channel: String,
    /// e.g. `25.05` or `unstable`
    pub series: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Datasets of this release that are contained in the bundle.
    #[serde(default)]
    pub datasets: Vec<Dataset>,
}

impl Release {
    pub fn add_dataset(&mut self, d: Dataset) {
        if !self.datasets.contains(&d) {
            self.datasets.push(d);
            self.datasets.sort();
        }
    }
}

/// One closure of a release: the set of paths collected from one kind of
/// root (e.g. `store-paths`, `helpers-direct`, `helpers-full`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct Closure {
    /// Index into `Bundle::releases`.
    pub release: u32,
    pub closure_type: String,
}
compact_binary!(Closure {
    release,
    closure_type
});

/// An indexed store path, with its runtime references.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct ClosurePath {
    pub hash: StoreHash,
    pub name: String,
    /// Indices into `ClosureSection::paths`.
    #[serde(default)]
    pub references: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nar_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    /// Basename of the deriver (`<hash>-<name>.drv`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deriver: Option<String>,
    /// Indices into `ClosureSection::closures` (release indices in version 2).
    #[serde(alias = "releases")]
    pub closures: Bitset,
}
compact_binary!(ClosurePath {
    hash,
    name,
    references,
    nar_size,
    file_size,
    deriver,
    closures
});

/// A root of a closure that is not present in the cache.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct MissingPath {
    pub hash: StoreHash,
    pub name: String,
    /// Indices into `ClosureSection::closures` (release indices in version 2).
    #[serde(alias = "releases")]
    pub closures: Bitset,
}
compact_binary!(MissingPath { hash, name, closures });

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosureSection {
    /// The closures that path and missing bitsets refer to (empty in
    /// version 2 bundles, whose bits are release indices).
    #[serde(default)]
    pub closures: Vec<Closure>,
    pub paths: Vec<ClosurePath>,
    #[serde(default)]
    pub missing: Vec<MissingPath>,
}

impl ClosureSection {
    pub fn closure_index(&self, release: u32, closure_type: &str) -> Option<usize> {
        self.closures
            .iter()
            .position(|c| c.release == release && c.closure_type == closure_type)
    }

    /// Index of the closure, adding it to the table if absent.
    pub fn ensure_closure(&mut self, release: u32, closure_type: &str) -> usize {
        self.closure_index(release, closure_type).unwrap_or_else(|| {
            self.closures.push(Closure {
                release,
                closure_type: closure_type.to_string(),
            });
            self.closures.len() - 1
        })
    }
}

/// One evaluation of a release: a set of attributes evaluated in one way
/// (e.g. `helper-probes`). Evaluations are to eval errors what closures are
/// to missing paths.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct Eval {
    /// Index into `Bundle::releases`.
    pub release: u32,
    pub eval_type: String,
}
compact_binary!(Eval { release, eval_type });

/// An attribute that failed to evaluate. Evaluation is deterministic, so the
/// attribute and system identify the failure; the message is not kept.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct EvalError {
    pub attr: String,
    pub system: String,
    /// Indices into `EvalErrorsSection::evals`.
    pub evals: Bitset,
}
compact_binary!(EvalError { attr, system, evals });

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalErrorsSection {
    /// The evaluations that error bitsets refer to. An evaluation with no
    /// errors is listed too: it ran and everything evaluated.
    pub evals: Vec<Eval>,
    pub errors: Vec<EvalError>,
}

impl EvalErrorsSection {
    pub fn eval_index(&self, release: u32, eval_type: &str) -> Option<usize> {
        self.evals
            .iter()
            .position(|e| e.release == release && e.eval_type == eval_type)
    }

    /// Index of the evaluation, adding it to the table if absent.
    pub fn ensure_eval(&mut self, release: u32, eval_type: &str) -> usize {
        self.eval_index(release, eval_type).unwrap_or_else(|| {
            self.evals.push(Eval {
                release,
                eval_type: eval_type.to_string(),
            });
            self.evals.len() - 1
        })
    }

    /// Record a failed attribute in an evaluation.
    pub fn add_error(&mut self, attr: &str, system: &str, eval: usize) {
        match self
            .errors
            .iter_mut()
            .find(|e| e.attr == attr && e.system == system)
        {
            Some(e) => e.evals.set(eval),
            None => self.errors.push(EvalError {
                attr: attr.to_string(),
                system: system.to_string(),
                evals: Bitset::single(eval),
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct Package {
    /// nixpkgs attribute path.
    pub attr: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub system: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub license: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_program: Option<String>,
    /// Output names (without paths) as listed in packages.json.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub broken: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unfree: bool,
    pub releases: Bitset,
}
compact_binary!(Package {
    attr,
    name,
    pname,
    version,
    system,
    description,
    homepage,
    license,
    main_program,
    outputs,
    broken,
    unfree,
    releases
});

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackagesSection {
    pub entries: Vec<Package>,
}

/// Evaluated output paths for an attribute.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct Outpaths {
    pub attr: String,
    pub system: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drv_path: Option<String>,
    /// Output name to full store path.
    pub outputs: BTreeMap<String, String>,
    pub releases: Bitset,
}
compact_binary!(Outpaths {
    attr,
    system,
    drv_path,
    outputs,
    releases
});

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutpathsSection {
    pub entries: Vec<Outpaths>,
}

/// A program (binary name) provided by a package, from programs.sqlite.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct Program {
    pub name: String,
    pub system: String,
    /// Attribute providing the program.
    pub attr: String,
    pub releases: Bitset,
}
compact_binary!(Program {
    name,
    system,
    attr,
    releases
});

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramsSection {
    pub entries: Vec<Program>,
}

/// A multi-section interchange bundle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    pub format_version: u32,
    pub releases: Vec<Release>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closure: Option<ClosureSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packages: Option<PackagesSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outpaths: Option<OutpathsSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub programs: Option<ProgramsSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_errors: Option<EvalErrorsSection>,
}

impl Default for Bundle {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            releases: Vec::new(),
            closure: None,
            packages: None,
            outpaths: None,
            programs: None,
            eval_errors: None,
        }
    }
}

impl Bundle {
    pub fn release_index(&self, name: &str) -> Option<usize> {
        self.releases.iter().position(|r| r.name == name)
    }

    /// Index of the release, adding it to the table if absent.
    pub fn ensure_release(&mut self, release: Release) -> usize {
        if let Some(i) = self.release_index(&release.name) {
            for d in release.datasets {
                self.releases[i].add_dataset(d);
            }
            i
        } else {
            self.releases.push(release);
            self.releases.len() - 1
        }
    }

    pub fn sections(&self) -> Vec<Dataset> {
        let mut v = Vec::new();
        if self.closure.is_some() {
            v.push(Dataset::Closure);
        }
        if self.packages.is_some() {
            v.push(Dataset::Packages);
        }
        if self.outpaths.is_some() {
            v.push(Dataset::Outpaths);
        }
        if self.programs.is_some() {
            v.push(Dataset::Programs);
        }
        if self.eval_errors.is_some() {
            v.push(Dataset::EvalErrors);
        }
        v
    }

    /// Convert a version 2 bundle to the current format. Version 2 closure
    /// bits are release indices; each release with the closure dataset
    /// becomes one closure of type `v2_closure_type`.
    pub fn upgrade(&mut self, v2_closure_type: &str) {
        if self.format_version >= 3 {
            return;
        }
        if let Some(c) = &mut self.closure {
            let mut map = vec![None; self.releases.len()];
            c.closures.clear();
            for (i, r) in self.releases.iter().enumerate() {
                if r.datasets.contains(&Dataset::Closure) {
                    map[i] = Some(c.closures.len());
                    c.closures.push(Closure {
                        release: i as u32,
                        closure_type: v2_closure_type.to_string(),
                    });
                }
            }
            let remap = |i: usize| map.get(i).copied().flatten();
            for p in &mut c.paths {
                p.closures = p.closures.remap(remap);
            }
            for m in &mut c.missing {
                m.closures = m.closures.remap(remap);
            }
        }
        self.format_version = FORMAT_VERSION;
    }

    /// Check internal consistency (reference indices, bitset ranges).
    pub fn validate(&self) -> Result<(), String> {
        if self.format_version != FORMAT_VERSION {
            return Err(format!(
                "format version {} (expected {FORMAT_VERSION}; upgrade before use)",
                self.format_version
            ));
        }
        let n = self.releases.len();
        let check_bits = |b: &Bitset, what: &str| -> Result<(), String> {
            match b.iter().last() {
                Some(i) if i >= n => Err(format!("{what}: release bit {i} out of range ({n} releases)")),
                _ => Ok(()),
            }
        };
        let mut names = std::collections::HashSet::new();
        for r in &self.releases {
            if !names.insert(&r.name) {
                return Err(format!("duplicate release {}", r.name));
            }
        }
        if let Some(c) = &self.closure {
            let nc = c.closures.len();
            let mut seen = std::collections::HashSet::new();
            for cl in &c.closures {
                if cl.release as usize >= n {
                    return Err(format!(
                        "closure {cl:?}: release index out of range ({n} releases)"
                    ));
                }
                if !self.releases[cl.release as usize]
                    .datasets
                    .contains(&Dataset::Closure)
                {
                    return Err(format!(
                        "closure {cl:?}: release {} lacks the closure dataset",
                        self.releases[cl.release as usize].name
                    ));
                }
                if cl.closure_type.is_empty() || !seen.insert((cl.release, &cl.closure_type)) {
                    return Err(format!("closure {cl:?}: empty or duplicate closure type"));
                }
            }
            let check_closure_bits = |b: &Bitset, what: &str| -> Result<(), String> {
                match b.iter().last() {
                    Some(i) if i >= nc => {
                        Err(format!("{what}: closure bit {i} out of range ({nc} closures)"))
                    }
                    _ => Ok(()),
                }
            };
            let len = c.paths.len() as u32;
            for p in &c.paths {
                check_closure_bits(&p.closures, &p.name)?;
                if let Some(r) = p.references.iter().find(|&&r| r >= len) {
                    return Err(format!("{}: reference index {r} out of range", p.name));
                }
            }
            // Closures must be closed: a path's references belong to every
            // closure the path belongs to.
            for p in &c.paths {
                for &r in &p.references {
                    let t = &c.paths[r as usize];
                    if !p.closures.is_subset(&t.closures) {
                        return Err(format!(
                            "{}-{} (closures {:?}) references {}-{} which is missing from some of them ({:?})",
                            p.hash, p.name, p.closures, t.hash, t.name, t.closures
                        ));
                    }
                }
            }
            for m in &c.missing {
                check_closure_bits(&m.closures, &m.name)?;
            }
        }
        if let Some(s) = &self.packages {
            for e in &s.entries {
                check_bits(&e.releases, &e.attr)?;
            }
        }
        if let Some(s) = &self.outpaths {
            for e in &s.entries {
                check_bits(&e.releases, &e.attr)?;
            }
        }
        if let Some(s) = &self.programs {
            for e in &s.entries {
                check_bits(&e.releases, &e.name)?;
            }
        }
        if let Some(s) = &self.eval_errors {
            let ne = s.evals.len();
            let mut seen = std::collections::HashSet::new();
            for ev in &s.evals {
                let Some(r) = self.releases.get(ev.release as usize) else {
                    return Err(format!("eval {ev:?}: release index out of range ({n} releases)"));
                };
                if !r.datasets.contains(&Dataset::EvalErrors) {
                    return Err(format!(
                        "eval {ev:?}: release {} lacks the eval-errors dataset",
                        r.name
                    ));
                }
                if ev.eval_type.is_empty() || !seen.insert((ev.release, &ev.eval_type)) {
                    return Err(format!("eval {ev:?}: empty or duplicate eval type"));
                }
            }
            let mut keys = std::collections::HashSet::new();
            for e in &s.errors {
                if !keys.insert((&e.attr, &e.system)) {
                    return Err(format!("duplicate eval error {}@{}", e.attr, e.system));
                }
                if let Some(i) = e.evals.iter().last().filter(|&i| i >= ne) {
                    return Err(format!(
                        "{}@{}: eval bit {i} out of range ({ne} evals)",
                        e.attr, e.system
                    ));
                }
            }
        }
        Ok(())
    }
}

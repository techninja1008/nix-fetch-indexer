//! Bundle data model.

use crate::bitset::Bitset;
use crate::store_path::StoreHash;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const FORMAT_VERSION: u32 = 3;

/// The eval type of the runtime closures of a release's `store-paths.xz`.
/// Nothing is evaluated: the paths come from Hydra's evaluation, so this is
/// a pseudo-evaluation with a single, unnamed closure type. It is also the
/// usual closure type to read version 2 bundles (which predate closure
/// types) as.
pub const STORE_PATHS: &str = "store-paths";

/// A closure type: a set of roots produced by an evaluation of type
/// `eval_type`. Closure type names are namespaced by eval type and may be
/// empty. Written `<eval type>` when the name is empty, else
/// `<eval type>:<name>` (e.g. `store-paths`, `helpers:direct`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClosureType {
    pub eval_type: String,
    pub name: String,
}

impl ClosureType {
    pub fn new(eval_type: &str, name: &str) -> Self {
        Self {
            eval_type: eval_type.to_string(),
            name: name.to_string(),
        }
    }
}

impl std::fmt::Display for ClosureType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.name.is_empty() {
            f.write_str(&self.eval_type)
        } else {
            write!(f, "{}:{}", self.eval_type, self.name)
        }
    }
}

impl std::str::FromStr for ClosureType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (e, n) = s.split_once(':').unwrap_or((s, ""));
        if e.is_empty() {
            return Err(format!("closure type {s:?}: empty eval type"));
        }
        Ok(Self::new(e, n))
    }
}

/// Selects closure types: `<eval type>` selects all of an eval type's
/// closure types, `<eval type>:<name>` one of them (`<eval type>:` the
/// unnamed one).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClosureSelector {
    pub eval_type: String,
    pub name: Option<String>,
}

impl ClosureSelector {
    pub fn matches(&self, eval_type: &str, name: &str) -> bool {
        self.eval_type == eval_type && self.name.as_deref().is_none_or(|n| n == name)
    }
}

impl std::fmt::Display for ClosureSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.name {
            None => f.write_str(&self.eval_type),
            Some(n) => write!(f, "{}:{n}", self.eval_type),
        }
    }
}

impl std::str::FromStr for ClosureSelector {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (e, n) = match s.split_once(':') {
            Some((e, n)) => (e, Some(n.to_string())),
            None => (s, None),
        };
        if e.is_empty() {
            return Err(format!("closure selector {s:?}: empty eval type"));
        }
        Ok(Self {
            eval_type: e.to_string(),
            name: n,
        })
    }
}

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

/// One evaluation of a release: the release evaluated in one way (e.g.
/// `helpers`, or the `store-paths` pseudo-evaluation). Closures and eval
/// errors belong to evaluations.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct Eval {
    /// Index into `Bundle::releases`.
    pub release: u32,
    pub eval_type: String,
}
compact_binary!(Eval { release, eval_type });

/// One closure of an evaluation: the paths reachable from one set of roots
/// it produced. The release is the evaluation's.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct Closure {
    /// Index into `Bundle::evals`.
    pub eval: u32,
    /// Closure type name within the evaluation's eval type (may be empty).
    pub closure_type: String,
}
compact_binary!(Closure { eval, closure_type });

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
    pub fn closure_index(&self, eval: u32, closure_type: &str) -> Option<usize> {
        self.closures
            .iter()
            .position(|c| c.eval == eval && c.closure_type == closure_type)
    }

    /// Index of the closure, adding it to the table if absent.
    pub fn ensure_closure(&mut self, eval: u32, closure_type: &str) -> usize {
        self.closure_index(eval, closure_type).unwrap_or_else(|| {
            self.closures.push(Closure {
                eval,
                closure_type: closure_type.to_string(),
            });
            self.closures.len() - 1
        })
    }
}

/// An attribute that failed to evaluate. Evaluation is deterministic, so the
/// attribute and system identify the failure; the message is not kept.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct EvalError {
    pub attr: String,
    pub system: String,
    /// Indices into `Bundle::evals`.
    pub evals: Bitset,
}
compact_binary!(EvalError { attr, system, evals });

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalErrorsSection {
    /// The evaluations whose errors the section holds (indices into
    /// `Bundle::evals`). A covered evaluation without errors had none; for
    /// an evaluation not covered, the bundle says nothing about errors.
    pub evals: Bitset,
    pub errors: Vec<EvalError>,
}

impl EvalErrorsSection {
    /// Record a failed attribute in an evaluation, covering it.
    pub fn add_error(&mut self, attr: &str, system: &str, eval: usize) {
        self.evals.set(eval);
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
    /// Evaluations referred to by closures and eval errors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evals: Vec<Eval>,
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
            evals: Vec::new(),
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

    /// The type of a closure of this bundle.
    pub fn closure_type(&self, c: &Closure) -> ClosureType {
        let eval_type = self
            .evals
            .get(c.eval as usize)
            .map(|e| e.eval_type.as_str())
            .unwrap_or_default();
        ClosureType::new(eval_type, &c.closure_type)
    }

    /// The release a closure belongs to (through its evaluation).
    pub fn closure_release(&self, c: &Closure) -> Option<usize> {
        self.evals.get(c.eval as usize).map(|e| e.release as usize)
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
    /// gets an evaluation and closure of type `v2_closure_type`.
    pub fn upgrade(&mut self, v2_closure_type: &ClosureType) {
        if self.format_version >= 3 {
            return;
        }
        if let Some(mut c) = self.closure.take() {
            c.closures.clear();
            let with_closure: Vec<bool> = self
                .releases
                .iter()
                .map(|r| r.datasets.contains(&Dataset::Closure))
                .collect();
            let map: Vec<Option<usize>> = with_closure
                .iter()
                .enumerate()
                .map(|(i, &has)| {
                    has.then(|| {
                        let e = self.ensure_eval(i as u32, &v2_closure_type.eval_type);
                        c.ensure_closure(e as u32, &v2_closure_type.name)
                    })
                })
                .collect();
            let remap = |i: usize| map.get(i).copied().flatten();
            for p in &mut c.paths {
                p.closures = p.closures.remap(remap);
            }
            for m in &mut c.missing {
                m.closures = m.closures.remap(remap);
            }
            self.closure = Some(c);
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
        let ne = self.evals.len();
        let mut seen = std::collections::HashSet::new();
        for ev in &self.evals {
            if ev.release as usize >= n {
                return Err(format!("eval {ev:?}: release index out of range ({n} releases)"));
            }
            if ev.eval_type.is_empty() || !seen.insert((ev.release, &ev.eval_type)) {
                return Err(format!("eval {ev:?}: empty or duplicate eval type"));
            }
        }
        // The release of evaluation `e`, which must carry dataset `d`.
        let eval_release = |e: usize, d: Dataset, what: &str| -> Result<(), String> {
            let Some(ev) = self.evals.get(e) else {
                return Err(format!("{what}: eval index {e} out of range ({ne} evals)"));
            };
            let r = &self.releases[ev.release as usize];
            if !r.datasets.contains(&d) {
                return Err(format!("{what}: release {} lacks the {d} dataset", r.name));
            }
            Ok(())
        };
        if let Some(c) = &self.closure {
            let nc = c.closures.len();
            let mut seen = std::collections::HashSet::new();
            for cl in &c.closures {
                eval_release(cl.eval as usize, Dataset::Closure, &format!("closure {cl:?}"))?;
                if !seen.insert((cl.eval, &cl.closure_type)) {
                    return Err(format!("closure {cl:?}: duplicate closure type"));
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
            for e in s.evals.iter() {
                eval_release(e, Dataset::EvalErrors, &format!("eval errors of eval {e}"))?;
            }
            let mut keys = std::collections::HashSet::new();
            for e in &s.errors {
                if !keys.insert((&e.attr, &e.system)) {
                    return Err(format!("duplicate eval error {}@{}", e.attr, e.system));
                }
                if !e.evals.is_subset(&s.evals) {
                    return Err(format!(
                        "{}@{}: errors in evals {:?} whose errors the section does not cover ({:?})",
                        e.attr, e.system, e.evals, s.evals
                    ));
                }
            }
        }
        Ok(())
    }
}

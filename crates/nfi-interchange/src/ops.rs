//! Bundle operations: merge, filter/split, normalisation and statistics.

use crate::bitset::Bitset;
use crate::model::*;
use crate::store_path::StoreHash;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Builder for closure sections keyed by hash, with references as hashes.
/// Path and missing bits index the builder's closure table.
#[derive(Default)]
pub struct ClosureBuilder {
    closures: Vec<Closure>,
    index: HashMap<StoreHash, usize>,
    paths: Vec<(ClosurePath, Vec<StoreHash>)>,
    missing: HashMap<StoreHash, MissingPath>,
}

/// A store path's metadata, as fetched from a narinfo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathInfo {
    pub hash: StoreHash,
    pub name: String,
    pub references: Vec<StoreHash>,
    pub nar_size: Option<u64>,
    pub file_size: Option<u64>,
    pub deriver: Option<String>,
}

impl ClosureBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the builder from an existing closure section, remapping release
    /// indices (closures of releases mapped to `None` are dropped).
    pub fn from_section(section: &ClosureSection, remap: &dyn Fn(usize) -> Option<usize>) -> Self {
        let mut b = Self::new();
        b.extend_section(section, remap);
        b
    }

    pub fn extend_section(&mut self, section: &ClosureSection, remap: &dyn Fn(usize) -> Option<usize>) {
        let map: Vec<Option<usize>> = section
            .closures
            .iter()
            .map(|c| {
                remap(c.release as usize).map(|r| {
                    let i = self.ensure_closure(r, &c.closure_type);
                    self.add_skipped(i, &c.skipped);
                    i
                })
            })
            .collect();
        let cremap = |i: usize| map.get(i).copied().flatten();
        for p in &section.paths {
            let refs = p
                .references
                .iter()
                .map(|&r| section.paths[r as usize].hash)
                .collect();
            self.insert(
                PathInfo {
                    hash: p.hash,
                    name: p.name.clone(),
                    references: refs,
                    nar_size: p.nar_size,
                    file_size: p.file_size,
                    deriver: p.deriver.clone(),
                },
                &p.closures.remap(cremap),
            );
        }
        for m in &section.missing {
            let bits = m.closures.remap(cremap);
            if !bits.is_empty() {
                self.add_missing(m.hash, &m.name, &bits);
            }
        }
    }

    /// Index of the closure of `release` (a bundle release index) of the
    /// given type, adding it if absent.
    pub fn ensure_closure(&mut self, release: usize, closure_type: &str) -> usize {
        let release = release as u32;
        match self
            .closures
            .iter()
            .position(|c| c.release == release && c.closure_type == closure_type)
        {
            Some(i) => i,
            None => {
                self.closures.push(Closure {
                    release,
                    closure_type: closure_type.to_string(),
                    skipped: Vec::new(),
                });
                self.closures.len() - 1
            }
        }
    }

    /// Record roots of a closure that could not be collected.
    pub fn add_skipped(&mut self, closure: usize, skipped: &[String]) {
        let s = &mut self.closures[closure].skipped;
        for x in skipped {
            if !s.contains(x) {
                s.push(x.clone());
            }
        }
        s.sort();
    }

    pub fn closures(&self) -> &[Closure] {
        &self.closures
    }

    pub fn contains(&self, hash: &StoreHash) -> bool {
        self.index.contains_key(hash)
    }

    /// References (as hashes) of a known path.
    pub fn references(&self, hash: &StoreHash) -> Option<&[StoreHash]> {
        self.index.get(hash).map(|&i| self.paths[i].1.as_slice())
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Insert or update a path, OR-ing in the given closure bits.
    pub fn insert(&mut self, info: PathInfo, closures: &Bitset) {
        if let Some(&i) = self.index.get(&info.hash) {
            let (p, refs) = &mut self.paths[i];
            p.closures.union_with(closures);
            if refs.is_empty() && !info.references.is_empty() {
                *refs = info.references;
            }
            p.nar_size = p.nar_size.or(info.nar_size);
            p.file_size = p.file_size.or(info.file_size);
            if p.deriver.is_none() {
                p.deriver = info.deriver;
            }
        } else {
            self.index.insert(info.hash, self.paths.len());
            self.paths.push((
                ClosurePath {
                    hash: info.hash,
                    name: info.name,
                    references: Vec::new(),
                    nar_size: info.nar_size,
                    file_size: info.file_size,
                    deriver: info.deriver,
                    closures: closures.clone(),
                },
                info.references,
            ));
        }
    }

    /// Mark an already-known path as present in a closure.
    pub fn mark(&mut self, hash: &StoreHash, closure: usize) -> bool {
        match self.index.get(hash) {
            Some(&i) => {
                self.paths[i].0.closures.set(closure);
                true
            }
            None => false,
        }
    }

    pub fn add_missing(&mut self, hash: StoreHash, name: &str, closures: &Bitset) {
        self.missing
            .entry(hash)
            .or_insert_with(|| MissingPath {
                hash,
                name: name.to_string(),
                closures: Bitset::new(),
            })
            .closures
            .union_with(closures);
    }

    /// Mark the whole runtime closure of `roots` as present in `closure`.
    /// Returns the hashes of references that are not known to the builder.
    pub fn mark_closure(
        &mut self,
        roots: impl IntoIterator<Item = StoreHash>,
        closure: usize,
    ) -> Vec<StoreHash> {
        let mut unknown = Vec::new();
        let mut seen = HashSet::new();
        let mut stack: Vec<StoreHash> = roots.into_iter().collect();
        while let Some(h) = stack.pop() {
            if !seen.insert(h) {
                continue;
            }
            match self.index.get(&h) {
                Some(&i) => {
                    self.paths[i].0.closures.set(closure);
                    stack.extend(self.paths[i].1.iter().copied());
                }
                None => unknown.push(h),
            }
        }
        unknown
    }

    /// Produce a normalised section (paths sorted by hash). References to
    /// unknown paths are dropped.
    pub fn finish(self) -> ClosureSection {
        let mut paths = self.paths;
        paths.sort_by_key(|p| p.0.hash);
        let index: HashMap<StoreHash, u32> = paths
            .iter()
            .enumerate()
            .map(|(i, (p, _))| (p.hash, i as u32))
            .collect();
        let paths = paths
            .into_iter()
            .map(|(mut p, refs)| {
                let mut r: Vec<u32> = refs.iter().filter_map(|h| index.get(h).copied()).collect();
                r.sort_unstable();
                r.dedup();
                p.references = r;
                p
            })
            .collect();
        let mut missing: Vec<MissingPath> = self.missing.into_values().collect();
        missing.sort_by_key(|m| m.hash);
        ClosureSection {
            closures: self.closures,
            paths,
            missing,
        }
    }
}

/// Accumulates entries, OR-ing the release bits of entries with equal keys,
/// so many releases' entries can be collected without holding duplicates.
pub struct Dedup<K, T> {
    index: HashMap<K, usize>,
    entries: Vec<T>,
    key: fn(&T) -> K,
    bits: fn(&mut T) -> &mut Bitset,
}

impl<K: std::hash::Hash + Eq, T> Dedup<K, T> {
    pub fn add(&mut self, mut e: T) {
        let k = (self.key)(&e);
        match self.index.get(&k) {
            Some(&i) => {
                let bits = std::mem::take((self.bits)(&mut e));
                (self.bits)(&mut self.entries[i]).union_with(&bits);
            }
            None => {
                self.index.insert(k, self.entries.len());
                self.entries.push(e);
            }
        }
    }

    pub fn extend(&mut self, es: impl IntoIterator<Item = T>) {
        for e in es {
            self.add(e);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn into_entries(self) -> Vec<T> {
        self.entries
    }
}

impl Dedup<Package, Package> {
    pub fn packages() -> Self {
        Self {
            index: HashMap::new(),
            entries: Vec::new(),
            key: package_key,
            bits: |p| &mut p.releases,
        }
    }
}

impl Dedup<(String, String, Option<String>, BTreeMap<String, String>), Outpaths> {
    pub fn outpaths() -> Self {
        Self {
            index: HashMap::new(),
            entries: Vec::new(),
            key: outpaths_key,
            bits: |p| &mut p.releases,
        }
    }
}

impl Dedup<(String, String, String), Program> {
    pub fn programs() -> Self {
        Self {
            index: HashMap::new(),
            entries: Vec::new(),
            key: program_key,
            bits: |p| &mut p.releases,
        }
    }
}

fn package_key(p: &Package) -> Package {
    Package {
        releases: Bitset::new(),
        ..p.clone()
    }
}

fn outpaths_key(p: &Outpaths) -> (String, String, Option<String>, BTreeMap<String, String>) {
    (
        p.attr.clone(),
        p.system.clone(),
        p.drv_path.clone(),
        p.outputs.clone(),
    )
}

fn program_key(p: &Program) -> (String, String, String) {
    (p.name.clone(), p.system.clone(), p.attr.clone())
}

/// Merge `inputs` into a single bundle: union of release tables (by name),
/// union of entries (by key) with OR-ed release bits.
pub fn merge(inputs: impl IntoIterator<Item = Bundle>) -> Bundle {
    let mut out = Bundle::default();
    let mut closure: Option<ClosureBuilder> = None;
    let mut packages: Option<(HashMap<Package, usize>, Vec<Package>)> = None;
    let mut outpaths: Option<(HashMap<_, usize>, Vec<Outpaths>)> = None;
    let mut programs: Option<(HashMap<_, usize>, Vec<Program>)> = None;

    for input in inputs {
        let map: Vec<usize> = input
            .releases
            .iter()
            .map(|r| out.ensure_release(r.clone()))
            .collect();
        let remap = |i: usize| map.get(i).copied();
        if let Some(c) = &input.closure {
            closure
                .get_or_insert_with(ClosureBuilder::new)
                .extend_section(c, &remap);
        }
        if let Some(s) = input.packages {
            let (idx, v) = packages.get_or_insert_with(Default::default);
            for e in s.entries {
                let bits = e.releases.remap(remap);
                let k = package_key(&e);
                match idx.get(&k) {
                    Some(&i) => v[i].releases.union_with(&bits),
                    None => {
                        idx.insert(k, v.len());
                        v.push(Package { releases: bits, ..e });
                    }
                }
            }
        }
        if let Some(s) = input.outpaths {
            let (idx, v) = outpaths.get_or_insert_with(Default::default);
            for e in s.entries {
                let bits = e.releases.remap(remap);
                let k = outpaths_key(&e);
                match idx.get(&k) {
                    Some(&i) => v[i].releases.union_with(&bits),
                    None => {
                        idx.insert(k, v.len());
                        v.push(Outpaths { releases: bits, ..e });
                    }
                }
            }
        }
        if let Some(s) = input.programs {
            let (idx, v) = programs.get_or_insert_with(Default::default);
            for e in s.entries {
                let bits = e.releases.remap(remap);
                let k = program_key(&e);
                match idx.get(&k) {
                    Some(&i) => v[i].releases.union_with(&bits),
                    None => {
                        idx.insert(k, v.len());
                        v.push(Program { releases: bits, ..e });
                    }
                }
            }
        }
    }
    out.closure = closure.map(ClosureBuilder::finish);
    out.packages = packages.map(|(_, entries)| PackagesSection { entries });
    out.outpaths = outpaths.map(|(_, entries)| OutpathsSection { entries });
    out.programs = programs.map(|(_, entries)| ProgramsSection { entries });
    normalize(&mut out);
    out
}

/// Sort entries into a canonical order.
pub fn normalize(b: &mut Bundle) {
    if let Some(s) = &mut b.packages {
        s.entries.sort_by(|a, b| {
            (&a.attr, &a.system, &a.version, &a.name).cmp(&(&b.attr, &b.system, &b.version, &b.name))
        });
    }
    if let Some(s) = &mut b.outpaths {
        s.entries
            .sort_by(|a, b| (&a.attr, &a.system, &a.drv_path).cmp(&(&b.attr, &b.system, &b.drv_path)));
    }
    if let Some(s) = &mut b.programs {
        s.entries
            .sort_by(|a, b| (&a.name, &a.system, &a.attr).cmp(&(&b.name, &b.system, &b.attr)));
    }
}

/// Keep only the releases matching `keep`, the sections in `sections` and
/// the closures whose type is in `closure_types` (all when `None`). Entries
/// left without releases or closures are dropped; closure paths are kept
/// when referenced by a kept path.
pub fn filter(
    b: &Bundle,
    keep: impl Fn(&Release) -> bool,
    sections: Option<&[Dataset]>,
    closure_types: Option<&[String]>,
) -> Bundle {
    let want = |d: Dataset| sections.is_none_or(|s| s.contains(&d));
    let mut map = vec![None; b.releases.len()];
    let mut releases = Vec::new();
    for (i, r) in b.releases.iter().enumerate() {
        if keep(r) {
            map[i] = Some(releases.len());
            let mut r = r.clone();
            r.datasets.retain(|d| want(*d));
            releases.push(r);
        }
    }
    let remap = |i: usize| map.get(i).copied().flatten();
    let mut out = Bundle {
        releases,
        ..Bundle::default()
    };

    if let Some(c) = b.closure.as_ref().filter(|_| want(Dataset::Closure)) {
        let mut cmap = vec![None; c.closures.len()];
        let mut closures = Vec::new();
        for (j, cl) in c.closures.iter().enumerate() {
            let type_ok = closure_types.is_none_or(|t| t.contains(&cl.closure_type));
            if let Some(r) = remap(cl.release as usize).filter(|_| type_ok) {
                cmap[j] = Some(closures.len());
                closures.push(Closure {
                    release: r as u32,
                    ..cl.clone()
                });
            }
        }
        let cremap = |i: usize| cmap.get(i).copied().flatten();
        // Paths with remaining closure bits, plus everything they reference.
        let mut keep_idx = vec![false; c.paths.len()];
        let mut stack: Vec<usize> = c
            .paths
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.closures.remap(cremap).is_empty())
            .map(|(i, _)| i)
            .collect();
        while let Some(i) = stack.pop() {
            if std::mem::replace(&mut keep_idx[i], true) {
                continue;
            }
            stack.extend(
                c.paths[i]
                    .references
                    .iter()
                    .map(|&r| r as usize)
                    .filter(|&r| !keep_idx[r]),
            );
        }
        let mut newidx = vec![u32::MAX; c.paths.len()];
        let mut paths = Vec::new();
        for (i, p) in c.paths.iter().enumerate() {
            if keep_idx[i] {
                newidx[i] = paths.len() as u32;
                paths.push(ClosurePath {
                    closures: p.closures.remap(cremap),
                    ..p.clone()
                });
            }
        }
        for p in &mut paths {
            p.references = p.references.iter().map(|&r| newidx[r as usize]).collect();
        }
        let missing = c
            .missing
            .iter()
            .map(|m| MissingPath {
                closures: m.closures.remap(cremap),
                ..m.clone()
            })
            .filter(|m| !m.closures.is_empty())
            .collect();
        // A release keeps the closure dataset only while it has closures.
        for (i, r) in out.releases.iter_mut().enumerate() {
            if !closures.iter().any(|c| c.release as usize == i) {
                r.datasets.retain(|d| *d != Dataset::Closure);
            }
        }
        out.closure = Some(ClosureSection {
            closures,
            paths,
            missing,
        });
    }
    if let Some(s) = b.packages.as_ref().filter(|_| want(Dataset::Packages)) {
        out.packages = Some(PackagesSection {
            entries: s
                .entries
                .iter()
                .map(|e| Package {
                    releases: e.releases.remap(remap),
                    ..e.clone()
                })
                .filter(|e| !e.releases.is_empty())
                .collect(),
        });
    }
    if let Some(s) = b.outpaths.as_ref().filter(|_| want(Dataset::Outpaths)) {
        out.outpaths = Some(OutpathsSection {
            entries: s
                .entries
                .iter()
                .map(|e| Outpaths {
                    releases: e.releases.remap(remap),
                    ..e.clone()
                })
                .filter(|e| !e.releases.is_empty())
                .collect(),
        });
    }
    if let Some(s) = b.programs.as_ref().filter(|_| want(Dataset::Programs)) {
        out.programs = Some(ProgramsSection {
            entries: s
                .entries
                .iter()
                .map(|e| Program {
                    releases: e.releases.remap(remap),
                    ..e.clone()
                })
                .filter(|e| !e.releases.is_empty())
                .collect(),
        });
    }
    out
}

/// Split a bundle into one bundle per release.
pub fn split(b: &Bundle) -> Vec<Bundle> {
    b.releases
        .iter()
        .map(|r| filter(b, |x| x.name == r.name, None, None))
        .collect()
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ReleaseStats {
    pub name: String,
    pub commit: String,
    pub channel: String,
    pub series: String,
    pub datasets: Vec<Dataset>,
    pub closure_paths: usize,
    pub missing_paths: usize,
    pub packages: usize,
    pub outpaths: usize,
    pub programs: usize,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ClosureStats {
    pub release: String,
    pub closure_type: String,
    pub paths: usize,
    pub missing: usize,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Stats {
    pub format_version: u32,
    pub sections: Vec<Dataset>,
    pub closure_paths: usize,
    pub closure_edges: usize,
    pub missing_paths: usize,
    pub packages: usize,
    pub outpaths: usize,
    pub programs: usize,
    pub releases: Vec<ReleaseStats>,
    pub closures: Vec<ClosureStats>,
}

pub fn stats(b: &Bundle) -> Stats {
    let mut releases: Vec<ReleaseStats> = b
        .releases
        .iter()
        .map(|r| ReleaseStats {
            name: r.name.clone(),
            commit: r.commit.clone(),
            channel: r.channel.clone(),
            series: r.series.clone(),
            datasets: r.datasets.clone(),
            ..Default::default()
        })
        .collect();
    let mut s = Stats {
        format_version: b.format_version,
        sections: b.sections(),
        ..Default::default()
    };
    let n = releases.len();
    let mut count = |bits: &Bitset, f: fn(&mut ReleaseStats) -> &mut usize| {
        for i in bits.iter().filter(|&i| i < n) {
            *f(&mut releases[i]) += 1;
        }
    };
    if let Some(c) = &b.closure {
        s.closure_paths = c.paths.len();
        s.closure_edges = c.paths.iter().map(|p| p.references.len()).sum();
        s.missing_paths = c.missing.len();
        let mut closures: Vec<ClosureStats> = c
            .closures
            .iter()
            .map(|cl| ClosureStats {
                release: b
                    .releases
                    .get(cl.release as usize)
                    .map(|r| r.name.clone())
                    .unwrap_or_default(),
                closure_type: cl.closure_type.clone(),
                ..Default::default()
            })
            .collect();
        // Per release, a path counts once however many of its closures hold it.
        let releases_of =
            |bits: &Bitset| -> Bitset { bits.remap(|j| c.closures.get(j).map(|cl| cl.release as usize)) };
        for p in &c.paths {
            count(&releases_of(&p.closures), |r| &mut r.closure_paths);
            let n = closures.len();
            for j in p.closures.iter().filter(|&j| j < n) {
                closures[j].paths += 1;
            }
        }
        for m in &c.missing {
            count(&releases_of(&m.closures), |r| &mut r.missing_paths);
            let n = closures.len();
            for j in m.closures.iter().filter(|&j| j < n) {
                closures[j].missing += 1;
            }
        }
        s.closures = closures;
    }
    if let Some(p) = &b.packages {
        s.packages = p.entries.len();
        for e in &p.entries {
            count(&e.releases, |r| &mut r.packages);
        }
    }
    if let Some(p) = &b.outpaths {
        s.outpaths = p.entries.len();
        for e in &p.entries {
            count(&e.releases, |r| &mut r.outpaths);
        }
    }
    if let Some(p) = &b.programs {
        s.programs = p.entries.len();
        for e in &p.entries {
            count(&e.releases, |r| &mut r.programs);
        }
    }
    s.releases = releases;
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(attr: &str, release: usize) -> Program {
        Program {
            name: "hello".into(),
            system: "x86_64-linux".into(),
            attr: attr.into(),
            releases: Bitset::single(release),
        }
    }

    #[test]
    fn dedup_unions_releases() {
        let mut d = Dedup::programs();
        d.extend([program("hello", 0), program("hello", 2), program("hi", 1)]);
        assert_eq!(d.len(), 2);
        let entries = d.into_entries();
        assert_eq!(entries[0].releases.iter().collect::<Vec<_>>(), [0, 2]);
        assert_eq!(entries[1].releases.iter().collect::<Vec<_>>(), [1]);
    }
}

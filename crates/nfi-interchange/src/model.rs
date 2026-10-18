//! Bundle data model.

use crate::bitset::Bitset;
use crate::store_path::StoreHash;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const FORMAT_VERSION: u32 = 2;

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
#[serde(rename_all = "lowercase")]
pub enum Dataset {
    Closure,
    Packages,
    Outpaths,
    Programs,
}

impl Dataset {
    pub const ALL: [Dataset; 4] = [
        Dataset::Closure,
        Dataset::Packages,
        Dataset::Outpaths,
        Dataset::Programs,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Dataset::Closure => "closure",
            Dataset::Packages => "packages",
            Dataset::Outpaths => "outpaths",
            Dataset::Programs => "programs",
        }
    }
}

impl std::str::FromStr for Dataset {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Dataset::ALL.into_iter().find(|d| d.as_str() == s).ok_or_else(|| {
            format!("unknown dataset {s:?} (expected closure, packages, outpaths or programs)")
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
    pub releases: Bitset,
}
compact_binary!(ClosurePath {
    hash,
    name,
    references,
    nar_size,
    file_size,
    deriver,
    releases
});

/// A path listed in a release's store-paths but not present in the cache.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct MissingPath {
    pub hash: StoreHash,
    pub name: String,
    pub releases: Bitset,
}
compact_binary!(MissingPath { hash, name, releases });

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosureSection {
    pub paths: Vec<ClosurePath>,
    #[serde(default)]
    pub missing: Vec<MissingPath>,
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
        v
    }

    /// Check internal consistency (reference indices, bitset ranges).
    pub fn validate(&self) -> Result<(), String> {
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
            let len = c.paths.len() as u32;
            for p in &c.paths {
                check_bits(&p.releases, &p.name)?;
                if let Some(r) = p.references.iter().find(|&&r| r >= len) {
                    return Err(format!("{}: reference index {r} out of range", p.name));
                }
            }
            // Closures must be closed: a path's references belong to every
            // release the path belongs to.
            for p in &c.paths {
                for &r in &p.references {
                    let t = &c.paths[r as usize];
                    if !p.releases.is_subset(&t.releases) {
                        return Err(format!(
                            "{}-{} (releases {:?}) references {}-{} which is missing from some of them ({:?})",
                            p.hash, p.name, p.releases, t.hash, t.name, t.releases
                        ));
                    }
                }
            }
            for m in &c.missing {
                check_bits(&m.releases, &m.name)?;
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
        Ok(())
    }
}

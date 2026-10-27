//! Package URLs (<https://github.com/package-url/purl-spec>).
//!
//! Mirror requests name what to mirror by PURL. Nix store paths use the
//! `nix` type with the whole store path, percent-encoded, as the name and
//! nothing else: `pkg:nix/%2Fnix%2Fstore%2F<hash>-<name>`. The name may also
//! be just `/nix/store/<hash>`.

use nfi_interchange::store_path::STORE_DIR;
use nfi_interchange::{StoreHash, StorePath};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use std::collections::BTreeMap;
use std::fmt;

/// Characters left unencoded in PURL components.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'.')
    .remove(b'-')
    .remove(b'_')
    .remove(b'~');

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PurlError {
    #[error("not a package URL (must start with pkg:): {0:?}")]
    Scheme(String),
    #[error("package URL without a type or name: {0:?}")]
    Incomplete(String),
    #[error("invalid percent-encoding in {0:?}")]
    Encoding(String),
    #[error("unsupported package type {0:?} (only nix is supported)")]
    UnsupportedType(String),
    #[error("nix package URLs take only a store path as their name: {0:?}")]
    NixExtra(String),
    #[error("not a store path: {0:?}")]
    StorePath(String),
}

/// A parsed package URL.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Purl {
    /// Lower-cased type, e.g. `nix`.
    pub ty: String,
    pub namespace: Option<String>,
    pub name: String,
    pub version: Option<String>,
    pub qualifiers: BTreeMap<String, String>,
    pub subpath: Option<String>,
}

fn decode(s: &str) -> Result<String, PurlError> {
    percent_decode_str(s)
        .decode_utf8()
        .map(|c| c.into_owned())
        .map_err(|_| PurlError::Encoding(s.to_string()))
}

fn encode(s: &str) -> String {
    utf8_percent_encode(s, COMPONENT).to_string()
}

impl Purl {
    pub fn parse(s: &str) -> Result<Self, PurlError> {
        let rest = s
            .strip_prefix("pkg:")
            .ok_or_else(|| PurlError::Scheme(s.to_string()))?
            .trim_start_matches('/');
        let (rest, subpath) = match rest.split_once('#') {
            Some((r, sp)) => (r, Some(decode(sp.trim_matches('/'))?).filter(|s| !s.is_empty())),
            None => (rest, None),
        };
        let (rest, qualifiers) = match rest.split_once('?') {
            Some((r, q)) => {
                let mut m = BTreeMap::new();
                for kv in q.split('&').filter(|kv| !kv.is_empty()) {
                    let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                    m.insert(k.to_ascii_lowercase(), decode(v)?);
                }
                (r, m)
            }
            None => (rest, BTreeMap::new()),
        };
        let rest = rest.trim_end_matches('/');
        let (ty, path) = rest
            .split_once('/')
            .ok_or_else(|| PurlError::Incomplete(s.to_string()))?;
        let (path, version) = match path.rsplit_once('@') {
            Some((p, v)) => (p, Some(decode(v)?)),
            None => (path, None),
        };
        let (namespace, name) = match path.rsplit_once('/') {
            Some((ns, n)) => (
                Some(
                    ns.split('/')
                        .map(decode)
                        .collect::<Result<Vec<_>, _>>()?
                        .join("/"),
                ),
                n,
            ),
            None => (None, path),
        };
        let name = decode(name)?;
        if ty.is_empty() || name.is_empty() {
            return Err(PurlError::Incomplete(s.to_string()));
        }
        Ok(Purl {
            ty: ty.to_ascii_lowercase(),
            namespace,
            name,
            version,
            qualifiers,
            subpath,
        })
    }

    /// The `nix` PURL for a store path.
    pub fn nix(store_path: &str) -> Self {
        Purl {
            ty: "nix".into(),
            namespace: None,
            name: store_path.to_string(),
            version: None,
            qualifiers: BTreeMap::new(),
            subpath: None,
        }
    }

    /// For CEL policies: `{type, namespace, name, version, qualifiers, subpath}`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "type": self.ty,
            "namespace": self.namespace,
            "name": self.name,
            "version": self.version,
            "qualifiers": self.qualifiers,
            "subpath": self.subpath,
        })
    }

    /// The store path this `nix` PURL names.
    pub fn nix_target(&self) -> Result<NixTarget, PurlError> {
        if self.ty != "nix" {
            return Err(PurlError::UnsupportedType(self.ty.clone()));
        }
        if self.namespace.is_some()
            || self.version.is_some()
            || !self.qualifiers.is_empty()
            || self.subpath.is_some()
        {
            return Err(PurlError::NixExtra(self.to_string()));
        }
        NixTarget::parse(&self.name)
    }
}

impl fmt::Display for Purl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pkg:{}/", self.ty)?;
        if let Some(ns) = &self.namespace {
            for seg in ns.split('/') {
                write!(f, "{}/", encode(seg))?;
            }
        }
        f.write_str(&encode(&self.name))?;
        if let Some(v) = &self.version {
            write!(f, "@{}", encode(v))?;
        }
        for (i, (k, v)) in self.qualifiers.iter().enumerate() {
            write!(f, "{}{k}={}", if i == 0 { '?' } else { '&' }, encode(v))?;
        }
        if let Some(sp) = &self.subpath {
            write!(f, "#{}", sp.split('/').map(encode).collect::<Vec<_>>().join("/"))?;
        }
        Ok(())
    }
}

impl std::str::FromStr for Purl {
    type Err = PurlError;
    fn from_str(s: &str) -> Result<Self, PurlError> {
        Self::parse(s)
    }
}

/// A store path to mirror: its hash, and its name when the PURL gave one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NixTarget {
    pub hash: StoreHash,
    pub name: Option<String>,
}

impl NixTarget {
    /// `/nix/store/<hash>` or `/nix/store/<hash>-<name>`.
    pub fn parse(path: &str) -> Result<Self, PurlError> {
        let bad = || PurlError::StorePath(path.to_string());
        let base = path
            .strip_prefix(STORE_DIR)
            .and_then(|r| r.strip_prefix('/'))
            .ok_or_else(bad)?;
        if base.len() == 32 {
            return Ok(NixTarget {
                hash: base.parse().map_err(|_| bad())?,
                name: None,
            });
        }
        if base.contains('/') {
            return Err(bad());
        }
        let sp = StorePath::parse(path).map_err(|_| bad())?;
        Ok(NixTarget {
            hash: sp.hash,
            name: Some(sp.name),
        })
    }

    pub fn path(&self) -> String {
        match &self.name {
            Some(n) => format!("{STORE_DIR}/{}-{n}", self.hash),
            None => format!("{STORE_DIR}/{}", self.hash),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "/nix/store/0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-hello-2.12.3";

    #[test]
    fn nix_purls() {
        let s = Purl::nix(P).to_string();
        assert_eq!(
            s,
            "pkg:nix/%2Fnix%2Fstore%2F0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-hello-2.12.3"
        );
        let p = Purl::parse(&s).unwrap();
        assert_eq!(p.name, P);
        let t = p.nix_target().unwrap();
        assert_eq!(t.name.as_deref(), Some("hello-2.12.3"));
        assert_eq!(t.path(), P);
        let h = Purl::parse("pkg:nix/%2Fnix%2Fstore%2F0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b").unwrap();
        let t = h.nix_target().unwrap();
        assert_eq!(t.name, None);
        assert_eq!(t.hash.to_string(), "0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b");
        // Names with characters that need encoding round-trip.
        let odd = "/nix/store/0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-a+b=c?d";
        let s = Purl::nix(odd).to_string();
        assert_eq!(Purl::parse(&s).unwrap().nix_target().unwrap().path(), odd);
    }

    #[test]
    fn rejects() {
        assert!(matches!(Purl::parse("nix/x"), Err(PurlError::Scheme(_))));
        assert!(matches!(Purl::parse("pkg:nix"), Err(PurlError::Incomplete(_))));
        let e = Purl::parse("pkg:npm/left-pad@1.0").unwrap().nix_target();
        assert!(matches!(e, Err(PurlError::UnsupportedType(_))));
        let e = Purl::parse(&format!("{}@1", Purl::nix(P))).unwrap().nix_target();
        assert!(matches!(e, Err(PurlError::NixExtra(_))));
        let e = Purl::parse(&format!("{}?a=b", Purl::nix(P)))
            .unwrap()
            .nix_target();
        assert!(matches!(e, Err(PurlError::NixExtra(_))));
        for bad in [
            "/nix/store/short",
            "/tmp/0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-x",
            "/nix/store/0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-x/bin/x",
            "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-x",
        ] {
            assert!(NixTarget::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn generic_purls() {
        let p = Purl::parse("pkg:maven/org.apache%2Fx/commons-io@2.6?repo=central&a=b#src/main").unwrap();
        assert_eq!(p.ty, "maven");
        assert_eq!(p.namespace.as_deref(), Some("org.apache/x"));
        assert_eq!(p.name, "commons-io");
        assert_eq!(p.version.as_deref(), Some("2.6"));
        assert_eq!(p.qualifiers.get("repo").map(String::as_str), Some("central"));
        assert_eq!(p.subpath.as_deref(), Some("src/main"));
        assert_eq!(p.to_json()["type"], "maven");
    }
}

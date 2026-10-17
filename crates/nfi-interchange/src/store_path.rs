//! Store path hashes and names.

use crate::nixbase32;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

pub const STORE_DIR: &str = "/nix/store";

/// The 160-bit hash part of a store path. Serialized as 20 raw bytes in the
/// binary form and as the 32 character nix-base32 string in JSON.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StoreHash(pub [u8; 20]);

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("invalid store path hash: {0:?}")]
    Hash(String),
    #[error("invalid store path: {0:?}")]
    Path(String),
}

impl StoreHash {
    pub fn to_nix32(&self) -> String {
        nixbase32::encode(&self.0)
    }
}

impl FromStr for StoreHash {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, ParseError> {
        let bytes = nixbase32::decode(s, 20).ok_or_else(|| ParseError::Hash(s.to_string()))?;
        Ok(StoreHash(bytes.try_into().expect("20 bytes")))
    }
}

impl fmt::Display for StoreHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_nix32())
    }
}

impl fmt::Debug for StoreHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StoreHash({self})")
    }
}

impl Serialize for StoreHash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&self.to_nix32())
        } else {
            s.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for StoreHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if d.is_human_readable() {
            let s = String::deserialize(d)?;
            s.parse().map_err(serde::de::Error::custom)
        } else {
            struct V;
            impl<'de> serde::de::Visitor<'de> for V {
                type Value = StoreHash;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("20 bytes")
                }
                fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<StoreHash, E> {
                    v.try_into()
                        .map(StoreHash)
                        .map_err(|_| E::invalid_length(v.len(), &self))
                }
                fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<StoreHash, E> {
                    self.visit_bytes(&v)
                }
            }
            d.deserialize_bytes(V)
        }
    }
}

/// A parsed store path `/nix/store/<hash>-<name>`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct StorePath {
    pub hash: StoreHash,
    pub name: String,
}

impl StorePath {
    /// Parse either a full path (`/nix/store/<hash>-<name>`, trailing
    /// sub-paths are ignored) or a basename (`<hash>-<name>`).
    pub fn parse(s: &str) -> Result<Self, ParseError> {
        let base = s
            .strip_prefix(STORE_DIR)
            .map(|r| r.trim_start_matches('/'))
            .unwrap_or(s);
        let base = base.split('/').next().unwrap_or("");
        if base.len() < 34 || base.as_bytes()[32] != b'-' {
            return Err(ParseError::Path(s.to_string()));
        }
        let hash = base[..32].parse()?;
        let name = base[33..].to_string();
        if name.is_empty() {
            return Err(ParseError::Path(s.to_string()));
        }
        Ok(StorePath { hash, name })
    }

    pub fn basename(&self) -> String {
        format!("{}-{}", self.hash, self.name)
    }

    pub fn full(&self) -> String {
        format!("{STORE_DIR}/{}-{}", self.hash, self.name)
    }
}

impl fmt::Display for StorePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{STORE_DIR}/{}-{}", self.hash, self.name)
    }
}

impl FromStr for StorePath {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, ParseError> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_paths() {
        let p = StorePath::parse("/nix/store/0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-hello-2.12.3").unwrap();
        assert_eq!(p.name, "hello-2.12.3");
        assert_eq!(p.hash.to_string(), "0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b");
        assert_eq!(
            p.full(),
            "/nix/store/0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-hello-2.12.3"
        );
        let q = StorePath::parse("0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-hello-2.12.3").unwrap();
        assert_eq!(p, q);
        let r =
            StorePath::parse("/nix/store/0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-hello-2.12.3/bin/hello").unwrap();
        assert_eq!(p, r);
        assert!(StorePath::parse("/nix/store/short-name").is_err());
        assert!(StorePath::parse("/nix/store/0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-").is_err());
    }

    #[test]
    fn hash_serde() {
        let h: StoreHash = "0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b".parse().unwrap();
        let j = serde_json::to_string(&h).unwrap();
        assert_eq!(j, "\"0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b\"");
        assert_eq!(serde_json::from_str::<StoreHash>(&j).unwrap(), h);
        let mut buf = Vec::new();
        ciborium::into_writer(&h, &mut buf).unwrap();
        assert_eq!(buf.len(), 21);
        assert_eq!(ciborium::from_reader::<StoreHash, _>(&buf[..]).unwrap(), h);
    }
}

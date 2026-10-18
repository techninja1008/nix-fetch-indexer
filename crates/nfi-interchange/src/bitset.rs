//! Release membership bitsets.
//!
//! Bit `i` refers to `Bundle::releases[i]`. In the binary (CBOR) form a bitset
//! is a byte string (little-endian bit order); in JSON it is a hex string.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct Bitset(Vec<u8>);

impl Bitset {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn single(i: usize) -> Self {
        let mut b = Self::new();
        b.set(i);
        b
    }

    pub fn from_indices(indices: impl IntoIterator<Item = usize>) -> Self {
        let mut b = Self::new();
        for i in indices {
            b.set(i);
        }
        b
    }

    pub fn set(&mut self, i: usize) {
        let byte = i / 8;
        if self.0.len() <= byte {
            self.0.resize(byte + 1, 0);
        }
        self.0[byte] |= 1 << (i % 8);
    }

    pub fn clear(&mut self, i: usize) {
        if let Some(b) = self.0.get_mut(i / 8) {
            *b &= !(1 << (i % 8));
        }
        self.normalize();
    }

    pub fn get(&self, i: usize) -> bool {
        self.0.get(i / 8).is_some_and(|b| b & (1 << (i % 8)) != 0)
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&b| b == 0)
    }

    pub fn count(&self) -> usize {
        self.0.iter().map(|b| b.count_ones() as usize).sum()
    }

    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().enumerate().flat_map(|(byte, &b)| {
            (0..8)
                .filter(move |bit| b & (1 << bit) != 0)
                .map(move |bit| byte * 8 + bit)
        })
    }

    /// Whether every bit set in `self` is also set in `other`.
    pub fn is_subset(&self, other: &Bitset) -> bool {
        self.0
            .iter()
            .enumerate()
            .all(|(i, &b)| b & !other.0.get(i).copied().unwrap_or(0) == 0)
    }

    pub fn union_with(&mut self, other: &Bitset) {
        if self.0.len() < other.0.len() {
            self.0.resize(other.0.len(), 0);
        }
        for (a, b) in self.0.iter_mut().zip(&other.0) {
            *a |= b;
        }
    }

    /// Map each set bit `i` to `map(i)`; bits mapping to `None` are dropped.
    pub fn remap(&self, map: impl Fn(usize) -> Option<usize>) -> Bitset {
        Bitset::from_indices(self.iter().filter_map(map))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        let mut b = Self(bytes);
        b.normalize();
        b
    }

    fn normalize(&mut self) {
        while self.0.last() == Some(&0) {
            self.0.pop();
        }
    }
}

impl std::fmt::Debug for Bitset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl Serialize for Bitset {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&hex::encode(&self.0))
        } else {
            s.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Bitset {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if d.is_human_readable() {
            let s = String::deserialize(d)?;
            hex::decode(s)
                .map(Bitset::from_bytes)
                .map_err(serde::de::Error::custom)
        } else {
            struct V;
            impl<'de> serde::de::Visitor<'de> for V {
                type Value = Vec<u8>;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("a byte string")
                }
                fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
                    Ok(v.to_vec())
                }
                fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
                    Ok(v)
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut a: A) -> Result<Vec<u8>, A::Error> {
                    let mut v = Vec::new();
                    while let Some(b) = a.next_element()? {
                        v.push(b);
                    }
                    Ok(v)
                }
            }
            d.deserialize_bytes(V).map(Bitset::from_bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        let mut b = Bitset::new();
        assert!(b.is_empty());
        b.set(0);
        b.set(9);
        b.set(17);
        assert!(b.get(9) && !b.get(8));
        assert_eq!(b.iter().collect::<Vec<_>>(), vec![0, 9, 17]);
        assert_eq!(b.count(), 3);
        b.clear(17);
        assert_eq!(b.as_bytes().len(), 2);
        let r = b.remap(|i| if i == 0 { None } else { Some(i + 1) });
        assert_eq!(r.iter().collect::<Vec<_>>(), vec![10]);
        let mut u = Bitset::single(3);
        u.union_with(&b);
        assert_eq!(u.iter().collect::<Vec<_>>(), vec![0, 3, 9]);
    }

    #[test]
    fn serde_forms() {
        let b = Bitset::from_indices([1, 8]);
        assert_eq!(serde_json::to_string(&b).unwrap(), "\"0201\"");
        let back: Bitset = serde_json::from_str("\"0201\"").unwrap();
        assert_eq!(back, b);
        let mut buf = Vec::new();
        ciborium::into_writer(&b, &mut buf).unwrap();
        assert_eq!(buf, vec![0x42, 0x02, 0x01]);
        let back: Bitset = ciborium::from_reader(&buf[..]).unwrap();
        assert_eq!(back, b);
    }
}

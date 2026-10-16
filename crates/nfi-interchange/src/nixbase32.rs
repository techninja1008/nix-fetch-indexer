//! Nix's base-32 encoding (as used in store path hashes).

const ALPHABET: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// Length of the encoding of `n` bytes.
pub const fn encoded_len(n: usize) -> usize {
    (n * 8).div_ceil(5)
}

/// Encode bytes using Nix's base-32 variant (least significant digit last).
pub fn encode(bytes: &[u8]) -> String {
    let len = encoded_len(bytes.len());
    let mut out = String::with_capacity(len);
    for n in (0..len).rev() {
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        let lo = bytes[i] as u16 >> j;
        let hi = if i + 1 < bytes.len() {
            (bytes[i + 1] as u16) << (8 - j)
        } else {
            0
        };
        out.push(ALPHABET[((lo | hi) & 0x1f) as usize] as char);
    }
    out
}

fn digit(c: u8) -> Option<u8> {
    ALPHABET.iter().position(|&a| a == c).map(|p| p as u8)
}

/// Decode a Nix base-32 string into `out_len` bytes.
pub fn decode(s: &str, out_len: usize) -> Option<Vec<u8>> {
    if s.len() != encoded_len(out_len) {
        return None;
    }
    let mut out = vec![0u8; out_len];
    for (n, c) in s.bytes().rev().enumerate() {
        let d = digit(c)? as u16;
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        out[i] |= (d << j) as u8;
        let carry = d >> (8 - j);
        if i + 1 < out_len {
            out[i + 1] |= carry as u8;
        } else if carry != 0 {
            return None;
        }
    }
    Some(out)
}

/// Whether `s` is a valid store path hash (32 nix-base32 characters).
pub fn is_valid_hash(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|c| digit(c).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        // `nix hash convert --hash-algo sha1 --to nix32 2fd4e1c67a2d28fced849ee1bb76e7391b93eb12`
        let bytes = hex::decode("2fd4e1c67a2d28fced849ee1bb76e7391b93eb12").unwrap();
        assert_eq!(encode(&bytes), "2bmr66rrwxvbpqcyhknzqa1dgb3f3m1g");
    }

    #[test]
    fn roundtrip() {
        for seed in 0u8..50 {
            let bytes: Vec<u8> = (0..20)
                .map(|i| seed.wrapping_mul(31).wrapping_add(i * 7))
                .collect();
            let s = encode(&bytes);
            assert_eq!(s.len(), 32);
            assert!(is_valid_hash(&s));
            assert_eq!(decode(&s, 20).unwrap(), bytes);
        }
    }

    #[test]
    fn rejects_invalid() {
        assert!(decode("e", 20).is_none());
        assert!(decode("0000000000000000000000000000000u", 20).is_none());
        assert!(!is_valid_hash("0000000000000000000000000000000e"));
        // Overflowing top digit (only 160 bits available in 32 digits * 5 = 160, so no overflow here),
        // but for 1 byte -> 2 digits, the top digit may only use 3 bits.
        assert!(decode("z0", 1).is_none());
    }
}

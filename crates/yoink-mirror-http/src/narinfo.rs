//! narinfo parsing, rewriting and signing.

use anyhow::{Context, Result, bail};
use nfi_interchange::StorePath;
use nfi_interchange::store_path::STORE_DIR;

/// A narinfo, keeping its lines so that it can be republished unchanged
/// apart from the `URL` (and optionally an added signature).
#[derive(Clone, Debug)]
pub struct Narinfo {
    lines: Vec<(String, String)>,
    pub store_path: StorePath,
    pub url: String,
    pub compression: Option<String>,
    pub file_hash: Option<String>,
    pub file_size: Option<u64>,
    pub nar_hash: String,
    pub nar_size: u64,
    pub references: Vec<String>,
}

impl Narinfo {
    pub fn parse(text: &str) -> Result<Self> {
        let mut lines = Vec::new();
        for l in text.lines().filter(|l| !l.trim().is_empty()) {
            let (k, v) = l
                .split_once(':')
                .with_context(|| format!("malformed narinfo line {l:?}"))?;
            lines.push((k.trim().to_string(), v.trim().to_string()));
        }
        let get = |k: &str| lines.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
        let need = |k: &str| get(k).with_context(|| format!("narinfo has no {k}"));
        let store_path = StorePath::parse(&need("StorePath")?).context("narinfo StorePath")?;
        let parse_u64 = |k: &str, v: String| v.parse::<u64>().with_context(|| format!("narinfo {k} {v:?}"));
        Ok(Narinfo {
            store_path,
            url: need("URL")?,
            compression: get("Compression"),
            file_hash: get("FileHash"),
            file_size: get("FileSize").map(|v| parse_u64("FileSize", v)).transpose()?,
            nar_hash: need("NarHash")?,
            nar_size: parse_u64("NarSize", need("NarSize")?)?,
            references: get("References")
                .map(|r| r.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
            lines,
        })
    }

    /// The signed fingerprint: `1;<path>;<nar hash>;<nar size>;<refs>`.
    pub fn fingerprint(&self) -> Result<String> {
        let h = &self.nar_hash;
        if !(h.starts_with("sha256:") && h.len() == 7 + 52) {
            bail!("cannot sign a NarHash not in sha256 nix-base32 form: {h}");
        }
        let refs: Vec<String> = self
            .references
            .iter()
            .map(|r| format!("{STORE_DIR}/{r}"))
            .collect();
        Ok(format!(
            "1;{};{h};{};{}",
            self.store_path.full(),
            self.nar_size,
            refs.join(",")
        ))
    }

    /// Render with `URL` replaced and `extra_sigs` appended.
    pub fn render(&self, url: &str, extra_sigs: &[String]) -> String {
        let mut out = String::new();
        for (k, v) in &self.lines {
            let v = if k == "URL" { url } else { v };
            out.push_str(&format!("{k}: {v}\n"));
        }
        for s in extra_sigs {
            out.push_str(&format!("Sig: {s}\n"));
        }
        out
    }
}

/// An Ed25519 signing key in Nix's `name:base64(secret key)` format.
pub struct Signer {
    name: String,
    key: aws_lc_rs::signature::Ed25519KeyPair,
}

impl Signer {
    pub fn parse(s: &str) -> Result<Self> {
        use base64::Engine;
        let (name, b64) = s
            .trim()
            .split_once(':')
            .context("signing key must be name:base64")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .context("decoding signing key")?;
        if bytes.len() != 64 {
            bail!(
                "signing key must be 64 bytes (seed and public key), got {}",
                bytes.len()
            );
        }
        let key = aws_lc_rs::signature::Ed25519KeyPair::from_seed_and_public_key(&bytes[..32], &bytes[32..])
            .map_err(|e| anyhow::anyhow!("invalid signing key: {e}"))?;
        Ok(Signer {
            name: name.to_string(),
            key,
        })
    }

    /// The `Sig` value for a narinfo.
    pub fn sign(&self, n: &Narinfo) -> Result<String> {
        use base64::Engine;
        let sig = self.key.sign(n.fingerprint()?.as_bytes());
        Ok(format!(
            "{}:{}",
            self.name,
            base64::engine::general_purpose::STANDARD.encode(sig.as_ref())
        ))
    }

    /// `name:base64(public key)`, as in `trusted-public-keys`.
    pub fn public_key(&self) -> String {
        use aws_lc_rs::signature::KeyPair;
        use base64::Engine;
        format!(
            "{}:{}",
            self.name,
            base64::engine::general_purpose::STANDARD.encode(self.key.public_key().as_ref())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NI: &str = "StorePath: /nix/store/0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-hello-2.12.3
URL: nar/1v3bj.nar.xz
Compression: xz
FileHash: sha256:1v3bjsk3gm5rsnf0hxfz2ycjxyxb9qyfh7x0jrlz0arw9hdbxpbv
FileSize: 50160
NarHash: sha256:0n6bvklrb7j1ii3pn8qkkmqiss1q1xmv9wwqf6hll7jrsbc6zp3c
NarSize: 226512
References: 0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-hello-2.12.3 1xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-glibc-2.44
Deriver: 2xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-hello-2.12.3.drv
Sig: cache.nixos.org-1:abc==
";

    #[test]
    fn parse_render() {
        let n = Narinfo::parse(NI).unwrap();
        assert_eq!(n.store_path.name, "hello-2.12.3");
        assert_eq!((n.file_size, n.nar_size), (Some(50160), 226512));
        assert_eq!(n.references.len(), 2);
        let out = n.render("1v3bj.nar.xz", &["k:sig".into()]);
        assert!(out.contains("\nURL: 1v3bj.nar.xz\n"));
        assert!(out.contains("Sig: cache.nixos.org-1:abc==\nSig: k:sig\n"));
        assert_eq!(out.lines().count(), NI.lines().count() + 1);
        assert_eq!(
            n.fingerprint().unwrap(),
            "1;/nix/store/0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-hello-2.12.3;\
             sha256:0n6bvklrb7j1ii3pn8qkkmqiss1q1xmv9wwqf6hll7jrsbc6zp3c;226512;\
             /nix/store/0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-hello-2.12.3,\
             /nix/store/1xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-glibc-2.44"
        );
        assert!(Narinfo::parse("URL: x\n").is_err());
    }

    #[test]
    fn signs() {
        use base64::Engine;
        let seed = [7u8; 32];
        let kp = aws_lc_rs::signature::Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        use aws_lc_rs::signature::KeyPair;
        let mut sk = seed.to_vec();
        sk.extend_from_slice(kp.public_key().as_ref());
        let s = Signer::parse(&format!(
            "test-1:{}",
            base64::engine::general_purpose::STANDARD.encode(&sk)
        ))
        .unwrap();
        let n = Narinfo::parse(NI).unwrap();
        let sig = s.sign(&n).unwrap();
        let (name, b64) = sig.split_once(':').unwrap();
        assert_eq!(name, "test-1");
        let raw = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let pk =
            aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::ED25519, kp.public_key());
        pk.verify(n.fingerprint().unwrap().as_bytes(), &raw).unwrap();
        assert!(s.public_key().starts_with("test-1:"));
        assert!(Signer::parse("nocolon").is_err());
    }
}

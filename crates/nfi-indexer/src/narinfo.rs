//! narinfo parsing.

use anyhow::{Context, Result, bail};
use nfi_interchange::{PathInfo, StoreHash, StorePath};

pub fn parse(text: &str) -> Result<PathInfo> {
    let mut store_path = None;
    let mut references = Vec::new();
    let mut nar_size = None;
    let mut file_size = None;
    let mut deriver = None;
    for line in text.lines() {
        let Some((k, v)) = line.split_once(": ") else {
            continue;
        };
        match k {
            "StorePath" => store_path = Some(StorePath::parse(v.trim()).context("StorePath")?),
            "References" => {
                for r in v.split_whitespace() {
                    references.push(StorePath::parse(r).context("References")?.hash);
                }
            }
            "NarSize" => nar_size = v.trim().parse().ok(),
            "FileSize" => file_size = v.trim().parse().ok(),
            "Deriver" => {
                let v = v.trim();
                if v != "unknown-deriver" && !v.is_empty() {
                    deriver = Some(v.to_string());
                }
            }
            _ => {}
        }
    }
    let Some(sp) = store_path else {
        bail!("narinfo without StorePath")
    };
    Ok(PathInfo {
        hash: sp.hash,
        name: sp.name,
        references,
        nar_size,
        file_size,
        deriver,
    })
}

pub fn url(cache: &str, hash: &StoreHash) -> String {
    format!("{}/{}.narinfo", cache.trim_end_matches('/'), hash)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses() {
        let t = "StorePath: /nix/store/0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-hello-2.12.3\nURL: nar/x.nar.zst\nCompression: zstd\nFileHash: sha256:abc\nFileSize: 1234\nNarHash: sha256:def\nNarSize: 5678\nReferences: 0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-hello-2.12.3 1xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-glibc-2.44\nDeriver: 2xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-hello-2.12.3.drv\nSig: cache.nixos.org-1:xyz\n";
        let p = super::parse(t).unwrap();
        assert_eq!(p.name, "hello-2.12.3");
        assert_eq!(p.references.len(), 2);
        assert_eq!(p.nar_size, Some(5678));
        assert_eq!(p.file_size, Some(1234));
        assert_eq!(
            p.deriver.as_deref(),
            Some("2xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-hello-2.12.3.drv")
        );
        let empty =
            super::parse("StorePath: /nix/store/0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-x\nReferences: \n").unwrap();
        assert!(empty.references.is_empty());
        assert!(super::parse("garbage").is_err());
    }
}

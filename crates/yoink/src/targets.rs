//! What `yoink <TARGET>...` was asked to pull.

use anyhow::{Result, bail};
use std::collections::HashSet;
use yoink_api::Purl;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A package URL, passed to the service as given.
    Purl(String),
    /// A flake reference, evaluated locally and minimised against NFI.
    Flake(String),
}

/// Classify one argument: `pkg:…` is a PURL, anything with `#` (or `.`
/// alone) is a flake reference. `name@version` is reserved for npm-style
/// resolution, which is not implemented yet.
pub fn classify(arg: &str) -> Result<Target> {
    if arg.starts_with("pkg:") {
        let p = Purl::parse(arg)?;
        if p.ty == "nix" {
            p.nix_target()?;
        }
        return Ok(Target::Purl(arg.to_string()));
    }
    if arg.contains('#') || arg == "." {
        return Ok(Target::Flake(arg.to_string()));
    }
    if arg.contains('@') {
        bail!(
            "{arg:?}: name@version targets are not yet supported; use a flake reference (e.g. nixpkgs#hello)"
        );
    }
    bail!(
        "{arg:?} is not a target: give a flake reference with an attribute (e.g. nixpkgs#hello or .#foo), \
         `.` for the current flake's default package, or a package URL (pkg:nix/…)"
    )
}

/// Drop repeated PURLs, keeping the first occurrence.
pub fn dedupe(purls: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    purls.into_iter().filter(|p| seen.insert(p.clone())).collect()
}

/// PURLs for a minimise closure, sorted by store path name, then hash.
pub fn closure_purls(closure: &[String]) -> Vec<String> {
    fn key(p: &str) -> (&str, &str) {
        let base = p.rsplit('/').next().unwrap_or(p);
        match base.split_once('-') {
            Some((hash, name)) => (name, hash),
            None => ("", base),
        }
    }
    let mut paths: Vec<&String> = closure.iter().collect();
    paths.sort_by(|a, b| key(a).cmp(&key(b)));
    paths.dedup();
    paths.into_iter().map(|p| Purl::nix(p).to_string()).collect()
}

/// A short display name for a PURL: the store path name, else the hash, else
/// the PURL itself.
pub fn display_name(purl: &str) -> String {
    match Purl::parse(purl).ok().and_then(|p| p.nix_target().ok()) {
        Some(t) => t.name.unwrap_or_else(|| t.hash.to_string()),
        None => purl.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "0xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b";

    #[test]
    fn classification() {
        let p = Purl::nix(&format!("/nix/store/{H}-hello")).to_string();
        assert_eq!(classify(&p).unwrap(), Target::Purl(p.clone()));
        assert_eq!(
            classify("pkg:npm/x@1").unwrap(),
            Target::Purl("pkg:npm/x@1".into())
        );
        assert!(classify("pkg:nix/%2Ftmp%2Fx").is_err());
        assert_eq!(
            classify("nixpkgs#hello").unwrap(),
            Target::Flake("nixpkgs#hello".into())
        );
        assert_eq!(
            classify("git+ssh://git@host/r#pkg").unwrap(),
            Target::Flake("git+ssh://git@host/r#pkg".into())
        );
        assert_eq!(classify(".").unwrap(), Target::Flake(".".into()));
        let e = classify("hello@1.2").unwrap_err().to_string();
        assert!(e.contains("not yet supported"), "{e}");
        for bad in ["nixpkgs", "github:o/r", "./foo", "hello"] {
            assert!(
                !classify(bad).unwrap_err().to_string().contains("not yet"),
                "{bad}"
            );
        }
    }

    #[test]
    fn dedupes_and_sorts() {
        assert_eq!(
            dedupe(["a", "b", "a", "c", "b"].map(String::from)),
            ["a", "b", "c"].map(String::from)
        );
        let h2 = "1xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b";
        let closure = vec![
            format!("/nix/store/{H}-zlib"),
            format!("/nix/store/{h2}-bash"),
            format!("/nix/store/{H}-bash"),
        ];
        let names: Vec<String> = closure_purls(&closure).iter().map(|p| display_name(p)).collect();
        assert_eq!(names, ["bash", "bash", "zlib"]);
        assert!(closure_purls(&closure)[0].contains(H));
        assert_eq!(
            display_name(&Purl::nix(&format!("/nix/store/{H}")).to_string()),
            H
        );
        assert_eq!(display_name("pkg:npm/x"), "pkg:npm/x");
    }
}

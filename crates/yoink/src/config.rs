//! Optional configuration file (`$XDG_CONFIG_HOME/yoink/config.toml`).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The yoink service.
    pub url: Option<String>,
    /// The nix-fetch-indexer service used to resolve flake references.
    pub nfi_url: Option<String>,
    /// nix command used to evaluate flake references.
    pub nix: Option<String>,
}

pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("yoink").join("config.toml"))
}

/// Load `path`, or the default file if it exists.
pub fn load(path: Option<&Path>) -> Result<Config> {
    let (path, explicit) = match path {
        Some(p) => (p.to_path_buf(), true),
        None => match default_path() {
            Some(p) => (p, false),
            None => return Ok(Config::default()),
        },
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn loads() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.toml");
        std::fs::write(&p, "url = \"http://y\"\nnfi_url = \"http://n\"\n").unwrap();
        let c = super::load(Some(&p)).unwrap();
        assert_eq!(
            (c.url.as_deref(), c.nfi_url.as_deref()),
            (Some("http://y"), Some("http://n"))
        );
        std::fs::write(&p, "bogus = 1\n").unwrap();
        assert!(super::load(Some(&p)).is_err());
        assert!(super::load(Some(&d.path().join("missing"))).is_err());
    }
}

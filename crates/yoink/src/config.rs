//! Optional configuration file (`$XDG_CONFIG_HOME/yoink/config.toml`).

use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

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

/// Load `path`, or the default file if it exists.
pub fn load(path: Option<&Path>) -> Result<Config> {
    nfi::load_toml("yoink", path)
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

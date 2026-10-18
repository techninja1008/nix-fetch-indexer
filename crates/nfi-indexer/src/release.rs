//! Release discovery on releases.nixos.org.

use crate::fetch::Fetcher;
use anyhow::{Context, Result, bail};
use nfi_interchange::Release;

pub const DEFAULT_RELEASES_BASE: &str = "https://releases.nixos.org";
pub const DEFAULT_S3_LIST_BASE: &str = "https://nix-releases.s3.amazonaws.com";

/// A release located at `<base>/<project>/<series dir>/<name>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseLocation {
    /// Full URL of the release directory (no trailing slash).
    pub url: String,
    pub project: String,
    pub series_dir: String,
    pub name: String,
}

impl ReleaseLocation {
    pub fn parse(url: &str) -> Result<Self> {
        let url = url.trim_end_matches('/').to_string();
        let mut segs = url.rsplit('/');
        let name = segs.next().unwrap_or_default().to_string();
        let series_dir = segs.next().unwrap_or_default().to_string();
        let project = segs.next().unwrap_or_default().to_string();
        if name.is_empty() || series_dir.is_empty() || project.is_empty() || !url.contains("://") {
            bail!("release URL must look like <base>/<project>/<series>/<release-name>, got {url:?}");
        }
        Ok(Self {
            url,
            project,
            series_dir,
            name,
        })
    }

    /// Channel name, e.g. `nixos-25.05`, `nixos-unstable`, `nixpkgs-unstable`.
    pub fn channel(&self) -> String {
        format!("{}-{}", self.project, self.series_dir)
    }

    pub fn file(&self, f: &str) -> String {
        format!("{}/{}", self.url, f)
    }

    /// Fetch `git-revision` and build the release table entry.
    pub async fn resolve(&self, fetcher: &Fetcher) -> Result<Release> {
        let commit = fetcher
            .get_text(&self.file("git-revision"))
            .await?
            .trim()
            .to_string();
        if commit.len() != 40 || !commit.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!("{}: unexpected git-revision {commit:?}", self.url);
        }
        Ok(Release {
            name: self.name.clone(),
            commit,
            channel: self.channel(),
            series: self.series_dir.clone(),
            url: Some(self.url.clone()),
            datasets: Vec::new(),
        })
    }
}

/// Sort key for release names: the revision count, e.g. 813814 in
/// `nixos-25.05.813814.ac62194c3917` or 880000 in `nixos-26.11pre880000.abcdef`.
pub fn revcount(name: &str) -> u64 {
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() < 2 {
        return 0;
    }
    // Stable: `nixos-25.05.813814.<hash>`; pre-releases put the count after
    // a marker: `nixos-26.11pre880000.<hash>`, `nixos-25.05beta772522.<hash>`.
    let candidate = parts[parts.len() - 2];
    let start = candidate
        .rfind(|c: char| !c.is_ascii_digit())
        .map_or(0, |i| i + 1);
    let digits = &candidate[start..];
    digits.parse().unwrap_or(0)
}

/// List all releases under a channel prefix (e.g. `nixos/25.05`) using the
/// S3 ListObjects API, ordered oldest first.
pub async fn list_channel(
    fetcher: &Fetcher,
    s3_base: &str,
    releases_base: &str,
    prefix: &str,
) -> Result<Vec<ReleaseLocation>> {
    let prefix = format!("{}/", prefix.trim_matches('/'));
    let mut names = list_children(fetcher, s3_base, &prefix)
        .await
        .context("listing releases")?;
    names.sort_by_key(|n| (revcount(n), n.clone()));
    let base = releases_base.trim_end_matches('/');
    names
        .into_iter()
        .map(|n| ReleaseLocation::parse(&format!("{base}/{prefix}{n}")))
        .collect()
}

/// List the immediate "directories" under an S3 prefix (e.g. the channels
/// under `nixos/`), sorted by name.
pub async fn list_children(fetcher: &Fetcher, s3_base: &str, prefix: &str) -> Result<Vec<String>> {
    let prefix = format!("{}/", prefix.trim_matches('/'));
    let mut marker: Option<String> = None;
    let mut names = Vec::new();
    loop {
        let mut url = format!("{s3_base}/?delimiter=/&prefix={prefix}");
        if let Some(m) = &marker {
            url.push_str(&format!("&marker={m}"));
        }
        let xml = fetcher.get_text(&url).await?;
        let page = parse_list(&xml);
        for p in &page.prefixes {
            let name = p.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            if !name.is_empty() && format!("{prefix}{name}/") == *p {
                names.push(name.to_string());
            }
        }
        if page.truncated {
            marker = page.next_marker.or_else(|| page.prefixes.last().cloned());
            if marker.is_none() {
                break;
            }
        } else {
            break;
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

pub struct ListPage {
    pub prefixes: Vec<String>,
    pub truncated: bool,
    pub next_marker: Option<String>,
}

fn tag_values<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        rest = &rest[i + open.len()..];
        match rest.find(&close) {
            Some(j) => {
                out.push(&rest[..j]);
                rest = &rest[j + close.len()..];
            }
            None => break,
        }
    }
    out
}

/// Minimal parser for S3 ListBucketResult XML.
pub fn parse_list(xml: &str) -> ListPage {
    let prefixes = tag_values(xml, "CommonPrefixes")
        .into_iter()
        .flat_map(|cp| tag_values(cp, "Prefix"))
        .map(|s| s.to_string())
        .collect();
    let truncated = tag_values(xml, "IsTruncated")
        .first()
        .is_some_and(|v| *v == "true");
    let next_marker = tag_values(xml, "NextMarker").first().map(|s| s.to_string());
    ListPage {
        prefixes,
        truncated,
        next_marker,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_location() {
        let l =
            ReleaseLocation::parse("https://releases.nixos.org/nixos/25.05/nixos-25.05.813814.ac62194c3917/")
                .unwrap();
        assert_eq!(l.name, "nixos-25.05.813814.ac62194c3917");
        assert_eq!(l.channel(), "nixos-25.05");
        assert_eq!(l.series_dir, "25.05");
        let u = ReleaseLocation::parse("https://releases.nixos.org/nixpkgs/unstable/nixpkgs-26.11pre123.abc")
            .unwrap();
        assert_eq!(u.channel(), "nixpkgs-unstable");
        assert!(ReleaseLocation::parse("nixos-25.05").is_err());
    }

    #[test]
    fn revcounts() {
        assert_eq!(revcount("nixos-25.05.813814.ac62194c3917"), 813814);
        assert_eq!(revcount("nixos-26.11pre880000.abcdef"), 880000);
        assert_eq!(revcount("nixos-25.05beta772522.1e5b653dff12"), 772522);
        assert!(
            revcount("nixos-25.05beta772522.1e5b653dff12") < revcount("nixos-26.11pre1092090.b25309931cfd")
        );
        assert_eq!(revcount("garbage"), 0);
    }

    #[test]
    fn s3_list() {
        let xml = r#"<?xml version="1.0"?><ListBucketResult><IsTruncated>true</IsTruncated><NextMarker>nixos/25.05/b/</NextMarker>
<Contents><Key>nixos/25.05/a</Key></Contents>
<CommonPrefixes><Prefix>nixos/25.05/a/</Prefix></CommonPrefixes><CommonPrefixes><Prefix>nixos/25.05/b/</Prefix></CommonPrefixes></ListBucketResult>"#;
        let p = parse_list(xml);
        assert_eq!(p.prefixes, vec!["nixos/25.05/a/", "nixos/25.05/b/"]);
        assert!(p.truncated);
        assert_eq!(p.next_marker.as_deref(), Some("nixos/25.05/b/"));
    }
}

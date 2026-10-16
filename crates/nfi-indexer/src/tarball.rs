//! nixexprs tarball handling.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Extract a `.tar.xz` into `dest` and return the single top-level directory.
pub fn extract(data: &[u8], dest: &Path) -> Result<PathBuf> {
    let mut ar = tar::Archive::new(xz2::read::XzDecoder::new(data));
    ar.set_preserve_permissions(false);
    ar.set_overwrite(true);
    ar.unpack(dest).context("extracting nixexprs tarball")?;
    let dirs: Vec<PathBuf> = std::fs::read_dir(dest)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    match dirs.as_slice() {
        [d] => Ok(d.clone()),
        _ => bail!(
            "expected one top-level directory in nixexprs tarball, found {}",
            dirs.len()
        ),
    }
}

/// Read a single file `<top>/<name>` out of a `.tar.xz` without extracting it all.
pub fn read_top_level_file(data: &[u8], name: &str) -> Result<Option<Vec<u8>>> {
    let mut ar = tar::Archive::new(xz2::read::XzDecoder::new(data));
    for entry in ar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let comps: Vec<_> = path.components().collect();
        if comps.len() == 2 && comps[1].as_os_str() == name {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            return Ok(Some(buf));
        }
    }
    Ok(None)
}

#[cfg(test)]
pub(crate) fn make_tar_xz(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(xz2::write::XzEncoder::new(Vec::new(), 1));
    for (name, data) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, name, *data).unwrap();
    }
    b.into_inner().unwrap().finish().unwrap()
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_and_extracts() {
        let t = super::make_tar_xz(&[
            ("top/a.txt", b"A"),
            ("top/sub/programs.sqlite", b"no"),
            ("top/programs.sqlite", b"yes"),
        ]);
        assert_eq!(
            super::read_top_level_file(&t, "programs.sqlite")
                .unwrap()
                .unwrap(),
            b"yes"
        );
        assert!(super::read_top_level_file(&t, "missing").unwrap().is_none());
        let d = tempfile::tempdir().unwrap();
        let top = super::extract(&t, d.path()).unwrap();
        assert!(top.ends_with("top"));
        assert_eq!(std::fs::read(top.join("a.txt")).unwrap(), b"A");
    }
}

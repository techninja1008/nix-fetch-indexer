use nfi_embedded_meilisearch::{EmbeddedMeilisearch, Options};
use std::io::{Read, Write};
use std::path::PathBuf;

fn binary() -> Option<PathBuf> {
    let b = std::env::var("NFI_TEST_MEILI_BIN")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var_os("PATH")?
                .to_str()?
                .split(':')
                .map(|d| PathBuf::from(d).join("meilisearch"))
                .find(|p| p.is_file())
        });
    if b.is_none() {
        assert!(
            std::env::var("NFI_TEST_REQUIRE_SERVICES").as_deref() != Ok("1"),
            "meilisearch not found"
        );
        eprintln!("skipping: meilisearch not found");
    }
    b
}

fn status(url: &str, path: &str, key: Option<&str>) -> u16 {
    let addr = url.trim_start_matches("http://");
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let auth = key
        .map(|k| format!("Authorization: Bearer {k}\r\n"))
        .unwrap_or_default();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut r = String::new();
    s.read_to_string(&mut r).unwrap();
    r[9..12].parse().unwrap()
}

fn alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap_or_default()
            .contains(") Z ")
}

#[test]
fn start_restart_and_version_reset() {
    let Some(bin) = binary() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut opts = Options::new(dir.path().join("meili"));
    opts.binary = Some(bin);

    let m = EmbeddedMeilisearch::start(&opts).unwrap();
    assert!(m.reset, "a new database needs a rebuild");
    assert_eq!(status(&m.url, "/health", None), 200);
    assert_eq!(
        status(&m.url, "/indexes", None),
        401,
        "master key must be enforced"
    );
    assert_eq!(status(&m.url, "/indexes", Some(&m.master_key)), 200);
    let pid = m.pid().unwrap();
    m.stop().unwrap();
    assert!(!alive(pid));

    let m = EmbeddedMeilisearch::start(&opts).unwrap();
    assert!(!m.reset, "same version reuses the database");
    let version = m.version.clone();
    drop(m);

    // A database from another version is discarded.
    let vfile = dir.path().join("meili/data.ms/VERSION");
    assert_eq!(std::fs::read_to_string(&vfile).unwrap().trim(), version);
    std::fs::write(&vfile, "0.1.0").unwrap();
    let m = EmbeddedMeilisearch::start(&opts).unwrap();
    assert!(m.reset);
    assert_eq!(std::fs::read_to_string(&vfile).unwrap().trim(), version);
}

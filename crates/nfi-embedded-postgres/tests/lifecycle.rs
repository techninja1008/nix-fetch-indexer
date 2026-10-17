use nfi_embedded_postgres::{EmbeddedPostgres, Options, PORT, USER, find_bin_dir};
use std::process::Command;

fn psql(bin: &std::path::Path, pg: &EmbeddedPostgres, sql: &str) -> String {
    let out = Command::new(bin.join("psql"))
        .arg("-h")
        .arg(pg.socket_dir())
        .args(["-p", &PORT.to_string(), "-U", USER, "-d", "nfi", "-tAc", sql])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap_or_default()
            .contains(") Z ")
}

#[test]
fn start_persist_restart_and_version_check() {
    // SAFETY: no preconditions.
    if unsafe { libc_geteuid() } == 0 {
        eprintln!("skipping: PostgreSQL cannot run as root");
        let dir = tempfile::tempdir().unwrap();
        let err = EmbeddedPostgres::start(&Options::new(dir.path())).err().unwrap();
        assert!(format!("{err:#}").contains("root"));
        return;
    }
    let bin = match find_bin_dir(
        std::env::var_os("NFI_TEST_PG_BINDIR")
            .map(std::path::PathBuf::from)
            .as_deref(),
    ) {
        Ok(b) => b,
        Err(e) => {
            assert!(
                std::env::var("NFI_TEST_REQUIRE_SERVICES").as_deref() != Ok("1"),
                "{e:#}"
            );
            eprintln!("skipping: {e:#}");
            return;
        }
    };
    let dir = tempfile::tempdir().unwrap();
    let mut opts = Options::new(dir.path().join("pg"));
    opts.bin_dir = Some(bin.clone());

    let pg = EmbeddedPostgres::start(&opts).unwrap();
    assert!(pg.created);
    assert!(pg.url().starts_with("postgres://nfi@localhost:5432/nfi?host=%2F"));
    psql(&bin, &pg, "CREATE TABLE t (x int); INSERT INTO t VALUES (42)");
    // No TCP listener.
    assert_eq!(psql(&bin, &pg, "SHOW listen_addresses"), "");
    let pid = pg.pid().unwrap();
    pg.stop().unwrap();
    assert!(!alive(pid));

    let pg = EmbeddedPostgres::start(&opts).unwrap();
    assert!(!pg.created);
    assert_eq!(psql(&bin, &pg, "SELECT x FROM t"), "42");
    // A second server on the same data directory is refused.
    assert!(EmbeddedPostgres::start(&opts).is_err());
    drop(pg);

    // A cluster from another major version is refused with guidance.
    let vfile = dir.path().join("pg/pgdata/PG_VERSION");
    std::fs::write(&vfile, "9\n").unwrap();
    let err = EmbeddedPostgres::start(&opts).err().unwrap();
    assert!(format!("{err:#}").contains("pg_upgrade"), "{err:#}");
}

unsafe extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

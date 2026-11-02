use nfi_embedded_postgres::{EmbeddedPostgres, Options, PORT, USER, find_bin_dir, is_root};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
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

/// The `nobody` user and group.
const NOBODY: u32 = 65534;

/// PostgreSQL refuses to run as root, so as root, check that starting fails
/// and run test `name` again in a child process of this test binary as
/// `nobody`. Returns whether it did (the caller then returns).
fn ran_unprivileged(name: &str) -> bool {
    if !is_root() {
        return false;
    }
    let dir = tempfile::tempdir().unwrap();
    let err = EmbeddedPostgres::start(&Options::new(dir.path())).err().unwrap();
    assert!(format!("{err:#}").contains("root"));
    let out = Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .current_dir("/")
        .uid(NOBODY)
        .gid(NOBODY)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!(
        "{name} as nobody:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{name} as nobody: {}", out.status);
    assert!(stdout.contains("1 passed"), "{name} did not run as nobody");
    true
}

/// The server binaries, or `None` to skip (unless services are required).
fn bin_dir() -> Option<PathBuf> {
    let explicit = std::env::var_os("NFI_TEST_PG_BINDIR").map(PathBuf::from);
    match find_bin_dir(explicit.as_deref()) {
        Ok(b) => Some(b),
        Err(e) => {
            assert!(
                std::env::var("NFI_TEST_REQUIRE_SERVICES").as_deref() != Ok("1"),
                "{e:#}"
            );
            eprintln!("skipping: {e:#}");
            None
        }
    }
}

#[test]
fn start_persist_restart_and_version_check() {
    if ran_unprivileged("start_persist_restart_and_version_check") {
        return;
    }
    let Some(bin) = bin_dir() else { return };
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

/// A `postmaster.pid` left behind by a server that is gone, naming a PID
/// now used by another process (here this one, as after a container
/// restart), does not stop the server from starting.
#[test]
fn stale_pid_file_with_live_pid() {
    if ran_unprivileged("stale_pid_file_with_live_pid") {
        return;
    }
    let Some(bin) = bin_dir() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut opts = Options::new(dir.path().join("pg"));
    opts.bin_dir = Some(bin.clone());
    EmbeddedPostgres::start(&opts).unwrap().stop().unwrap();

    let pgdata = dir.path().join("pg/pgdata");
    let me = std::process::id();
    std::fs::write(
        pgdata.join("postmaster.pid"),
        format!("{me}\n{}\n", pgdata.display()),
    )
    .unwrap();
    let pg = EmbeddedPostgres::start(&opts).unwrap();
    assert_ne!(pg.pid(), Some(me));
    assert_eq!(psql(&bin, &pg, "SELECT 1"), "1");
}

//! Test support: ephemeral Postgres and Meilisearch processes, and in-process
//! query service instances.
//!
//! Services are found via environment variables or the PATH:
//! - `NFI_TEST_DATABASE_URL`: use an existing Postgres server (admin URL)
//!   instead of spawning one. Otherwise `NFI_TEST_PG_BINDIR`, `pg_config
//!   --bindir`, or `/usr/lib/postgresql/*/bin` must provide `initdb`/`postgres`.
//! - `NFI_TEST_MEILI_URL`: use an existing Meilisearch. Otherwise
//!   `NFI_TEST_MEILI_BIN` or `meilisearch` on the PATH is spawned.
//!
//! When a service is unavailable the tests are skipped, unless
//! `NFI_TEST_REQUIRE_SERVICES=1` is set, in which case they fail.

use anyhow::{Context, Result, bail};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub struct Services {
    pub pg_admin_url: Option<String>,
    pub meili_url: Option<String>,
    _dir: Option<tempfile::TempDir>,
}

static SERVICES: OnceLock<Services> = OnceLock::new();
static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_port(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Kill `pid` once this test process exits.
fn watchdog(pid: u32, signal: &str) {
    let me = std::process::id();
    let _ = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "while kill -0 {me} 2>/dev/null; do sleep 1; done; kill -{signal} {pid} 2>/dev/null; sleep 5; kill -9 {pid} 2>/dev/null"
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn pg_bindir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("NFI_TEST_PG_BINDIR") {
        return Some(d.into());
    }
    if let Ok(out) = Command::new("pg_config").arg("--bindir").output() {
        let d = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        if d.join("initdb").exists() {
            return Some(d);
        }
    }
    if let Ok(entries) = std::fs::read_dir("/usr/lib/postgresql") {
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path().join("bin"))
            .collect();
        dirs.sort();
        if let Some(d) = dirs.into_iter().rev().find(|d| d.join("initdb").exists()) {
            return Some(d);
        }
    }
    which("initdb").and_then(|p| p.parent().map(Path::to_path_buf))
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

fn start_postgres(dir: &Path) -> Result<String> {
    let bindir = pg_bindir().context("postgres binaries not found")?;
    let data = dir.join("pg");
    let sock = dir.join("pgsock");
    std::fs::create_dir_all(&sock)?;
    let root = is_root();
    let cmd = |prog: &str| -> Command {
        if root {
            // Postgres refuses to run as root; use the postgres (or nobody) user.
            let user = if Command::new("id")
                .arg("postgres")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                "postgres"
            } else {
                "nobody"
            };
            let mut c = Command::new("runuser");
            c.args(["-u", user, "--"]).arg(bindir.join(prog));
            c
        } else {
            Command::new(bindir.join(prog))
        }
    };
    if root {
        let user = if Command::new("id")
            .arg("postgres")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            "postgres"
        } else {
            "nobody"
        };
        Command::new("chown").args(["-R", user]).arg(dir).status()?;
        Command::new("chmod").arg("755").arg(dir).status()?;
    }
    let out = cmd("initdb")
        .args(["-D"])
        .arg(&data)
        .args([
            "-U",
            "nfi",
            "--auth=trust",
            "--no-sync",
            "-E",
            "UTF8",
            "--locale=C",
        ])
        .output()?;
    if !out.status.success() {
        bail!("initdb failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let port = free_port();
    let child = cmd("postgres")
        .arg("-D")
        .arg(&data)
        .args([
            "-p",
            &port.to_string(),
            "-c",
            "listen_addresses=127.0.0.1",
            "-c",
            "fsync=off",
        ])
        .args([
            "-c",
            "synchronous_commit=off",
            "-c",
            "full_page_writes=off",
            "-c",
            "max_connections=300",
        ])
        .arg("-k")
        .arg(&sock)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning postgres")?;
    watchdog(child.id(), "INT");
    if !wait_port(port, Duration::from_secs(30)) {
        bail!("postgres did not start");
    }
    // The port opens slightly before connections are accepted.
    std::thread::sleep(Duration::from_millis(300));
    Ok(format!("postgres://nfi@127.0.0.1:{port}/postgres"))
}

fn start_meili(dir: &Path) -> Result<String> {
    let bin = std::env::var("NFI_TEST_MEILI_BIN")
        .map(PathBuf::from)
        .ok()
        .or_else(|| which("meilisearch"));
    let bin = bin.context("meilisearch binary not found")?;
    let port = free_port();
    let child = Command::new(bin)
        .args([
            "--http-addr",
            &format!("127.0.0.1:{port}"),
            "--no-analytics",
            "--env",
            "development",
        ])
        .arg("--db-path")
        .arg(dir.join("meili"))
        .arg("--dump-dir")
        .arg(dir.join("meili-dumps"))
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning meilisearch")?;
    watchdog(child.id(), "TERM");
    if !wait_port(port, Duration::from_secs(30)) {
        bail!("meilisearch did not start");
    }
    Ok(format!("http://127.0.0.1:{port}"))
}

fn require() -> bool {
    std::env::var("NFI_TEST_REQUIRE_SERVICES").as_deref() == Ok("1")
}

pub fn services() -> &'static Services {
    SERVICES.get_or_init(|| {
        let dir = tempfile::Builder::new()
            .prefix("nfi-testenv")
            .tempdir()
            .expect("tempdir");
        let pg = match std::env::var("NFI_TEST_DATABASE_URL") {
            Ok(u) => Some(u),
            Err(_) => match start_postgres(dir.path()) {
                Ok(u) => Some(u),
                Err(e) => {
                    if require() {
                        panic!("postgres unavailable: {e:#}");
                    }
                    eprintln!("postgres unavailable, skipping DB tests: {e:#}");
                    None
                }
            },
        };
        let meili = match std::env::var("NFI_TEST_MEILI_URL") {
            Ok(u) => Some(u),
            Err(_) => match start_meili(dir.path()) {
                Ok(u) => Some(u),
                Err(e) => {
                    if require() {
                        panic!("meilisearch unavailable: {e:#}");
                    }
                    eprintln!("meilisearch unavailable, skipping search tests: {e:#}");
                    None
                }
            },
        };
        Services {
            pg_admin_url: pg,
            meili_url: meili,
            _dir: Some(dir),
        }
    })
}

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    )
}

/// Create a fresh database and return its URL.
pub async fn fresh_database() -> Option<String> {
    let admin = services().pg_admin_url.clone()?;
    let name = unique("nfitest");
    let mut conn: sqlx::PgConnection = sqlx::Connection::connect(&admin)
        .await
        .expect("connect to test postgres");
    sqlx::Executor::execute(&mut conn, format!("CREATE DATABASE {name}").as_str())
        .await
        .expect("create database");
    let base = admin
        .rsplit_once('/')
        .map(|(b, _)| b.to_string())
        .unwrap_or(admin);
    Some(format!("{base}/{name}"))
}

/// A running in-process query service with public and admin listeners.
pub struct TestService {
    pub public_url: String,
    pub admin_url: String,
    pub state: nfi_service::AppState,
    pub has_search: bool,
}

/// Start a service on a fresh database. `admin_token` enables admin auth.
/// Returns `None` (test should be skipped) if Postgres is unavailable.
pub async fn start_service(admin_token: Option<&str>) -> Option<TestService> {
    let db = fresh_database().await?;
    let pool = nfi_service::connect(&db, 20).await.expect("migrate");
    let meili = services()
        .meili_url
        .as_deref()
        .map(|u| nfi_service::search::Meili::new(u, None, &unique("idx")).expect("meili client"));
    let has_search = meili.is_some();
    let state = nfi_service::AppState {
        pool,
        meili,
        admin_token: admin_token.map(str::to_string),
        http: reqwest::Client::new(),
        poll_interval: Duration::from_millis(100),
    };
    let public = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let public_url = format!("http://{}", public.local_addr().unwrap());
    let admin_url = format!("http://{}", admin.local_addr().unwrap());
    let (s1, s2) = (state.clone(), state.clone());
    tokio::spawn(async move { axum_serve(public, nfi_service::api::public_router(s1)).await });
    tokio::spawn(async move { axum_serve(admin, nfi_service::api::admin_router(s2)).await });
    tokio::spawn(nfi_service::jobs::worker(state.clone(), unique("worker")));
    Some(TestService {
        public_url,
        admin_url,
        state,
        has_search,
    })
}

async fn axum_serve(l: tokio::net::TcpListener, r: nfi_service::api::RouterAlias) {
    nfi_service::api::serve(l, r).await
}

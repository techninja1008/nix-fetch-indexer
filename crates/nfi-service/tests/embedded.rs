//! Single-node mode: nfi-service with embedded PostgreSQL and Meilisearch.

use nfi_api::*;
use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

/// The `nobody` user and group.
const NOBODY: u32 = 65534;

struct Node {
    child: Child,
    public: String,
    admin: String,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn start(dir: &Path, pg_bindir: &Path, meili: &Path) -> Node {
    let (p, a) = (free_port(), free_port());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nfi-service"));
    // PostgreSQL refuses to run as root: run the service as nobody then.
    if nfi_testenv::is_root() {
        cmd.uid(NOBODY).gid(NOBODY).current_dir("/");
    }
    let child = cmd
        .arg("--embedded-postgres")
        .arg(dir.join("pg"))
        .arg("--pg-bindir")
        .arg(pg_bindir)
        .arg("--embedded-meilisearch")
        .arg(dir.join("meili"))
        .arg("--meilisearch-bin")
        .arg(meili)
        .args([
            "--listen",
            &format!("127.0.0.1:{p}"),
            "--admin-listen",
            &format!("127.0.0.1:{a}"),
        ])
        .env("RUST_LOG", "info,sqlx=warn")
        .spawn()
        .unwrap();
    let mut node = Node {
        child,
        public: format!("http://127.0.0.1:{p}"),
        admin: format!("http://127.0.0.1:{a}"),
    };
    let http = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        assert!(
            node.child.try_wait().unwrap().is_none(),
            "nfi-service exited during startup"
        );
        if http
            .get(format!("{}/healthz", node.public))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return node;
        }
        assert!(Instant::now() < deadline, "nfi-service did not start");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Wait for the service to exit.
fn exit_status(node: &mut Node, within: Duration) -> ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(s) = node.child.try_wait().unwrap() {
            return s;
        }
        assert!(Instant::now() < deadline, "nfi-service did not exit");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn stop(mut node: Node, dir: &Path) {
    // SAFETY: signalling our own child.
    unsafe { libc_kill(node.child.id() as i32, 15) };
    let status = exit_status(&mut node, Duration::from_secs(90));
    assert!(status.success(), "nfi-service exited with {status}");
    // Both child servers shut down cleanly with it.
    assert!(
        !dir.join("pg/pgdata/postmaster.pid").exists(),
        "postgres still running"
    );
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

async fn jobs(http: &reqwest::Client, node: &Node) -> Vec<JobInfo> {
    http.get(format!("{}/admin/v1/jobs", node.admin))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn wait_idle(http: &reqwest::Client, node: &Node) -> Vec<JobInfo> {
    for _ in 0..600 {
        let j = jobs(http, node).await;
        if j.iter().all(|j| j.status.is_finished()) {
            return j;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("jobs did not finish");
}

async fn search_attrs(http: &reqwest::Client, node: &Node, q: &str) -> Vec<String> {
    let r: SearchResponse = http
        .get(format!("{}/v1/search?q={q}", node.public))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    r.hits.into_iter().map(|h| h.attr).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn single_node_lifecycle() {
    let started = Instant::now();
    let require = std::env::var("NFI_TEST_REQUIRE_SERVICES").as_deref() == Ok("1");
    let (Some(pg_bindir), Some(meili)) = (nfi_testenv::pg_bindir(), nfi_testenv::meili_bin()) else {
        assert!(!require, "postgres or meilisearch binaries not found");
        eprintln!("skipping: postgres or meilisearch binaries not found");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    if nfi_testenv::is_root() {
        // The service runs as nobody (see `start`); it keeps its data here.
        std::os::unix::fs::chown(dir, Some(NOBODY), Some(NOBODY)).unwrap();
        eprintln!("running nfi-service as nobody");
    }
    let http = reqwest::Client::new();

    // First start: fresh cluster and search index (a rebuild job is queued).
    let node = start(dir, &pg_bindir, &meili).await;
    let mut data = Vec::new();
    nfi_interchange::codec::write(&nfi_testenv::bundle(), &mut data, nfi_interchange::Format::Json).unwrap();
    let r = http
        .post(format!("{}/admin/v1/ingest", node.admin))
        .body(data)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let j = wait_idle(&http, &node).await;
    assert!(j.iter().all(|j| j.status == JobStatus::Succeeded), "{j:?}");
    let kinds: Vec<&str> = j.iter().rev().map(|j| j.kind.as_str()).collect();
    assert_eq!(kinds, vec!["reindex", "ingest"]);
    assert_eq!(search_attrs(&http, &node, "greeting").await, vec!["hello"]);
    let p: Value = http
        .get(format!(
            "{}/v1/packages/curl?commit={}",
            node.public,
            nfi_testenv::A
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(p["outputs"]["out"]["cached"], true);
    stop(node, dir);

    // Restart: data and search index persist; no rebuild needed.
    let node = start(dir, &pg_bindir, &meili).await;
    let rels: Vec<ReleaseInfo> = http
        .get(format!("{}/admin/v1/releases", node.admin))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rels.len(), 2);
    assert_eq!(search_attrs(&http, &node, "greeting").await, vec!["hello"]);
    assert_eq!(wait_idle(&http, &node).await.len(), 2);
    stop(node, dir);

    // A search database from another Meilisearch version is discarded and rebuilt.
    std::fs::write(dir.join("meili/data.ms/VERSION"), "0.1.0").unwrap();
    let node = start(dir, &pg_bindir, &meili).await;
    let j = wait_idle(&http, &node).await;
    assert_eq!(j.len(), 3);
    assert_eq!(j[0].kind, "reindex");
    assert_eq!(j[0].status, JobStatus::Succeeded, "{:?}", j[0].error);
    assert_eq!(search_attrs(&http, &node, "transfer").await, vec!["curl"]);
    stop(node, dir);

    // The service exits (failing) when an embedded server dies under it:
    // Meilisearch, then PostgreSQL.
    let mut node = start(dir, &pg_bindir, &meili).await;
    let pid = child_named(node.child.id(), "meilisearch").expect("meilisearch child");
    // SAFETY: signalling our child's child.
    unsafe { libc_kill(pid as i32, 9) };
    let status = exit_status(&mut node, Duration::from_secs(30));
    assert!(!status.success(), "nfi-service exited with {status}");
    assert!(
        !dir.join("pg/pgdata/postmaster.pid").exists(),
        "postgres still running"
    );

    let mut node = start(dir, &pg_bindir, &meili).await;
    let meili_pid = child_named(node.child.id(), "meilisearch").expect("meilisearch child");
    let pid: i32 = std::fs::read_to_string(dir.join("pg/pgdata/postmaster.pid"))
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: signalling our child's child.
    unsafe { libc_kill(pid, 9) };
    let status = exit_status(&mut node, Duration::from_secs(30));
    assert!(!status.success(), "nfi-service exited with {status}");
    assert!(!alive(meili_pid), "meilisearch still running");
    eprintln!("single_node_lifecycle ran in {:?}", started.elapsed());
}

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "))
}

/// A live child process of `parent` whose command name is `name`.
fn child_named(parent: u32, name: &str) -> Option<u32> {
    std::fs::read_dir("/proc").ok()?.find_map(|e| {
        let pid: u32 = e.ok()?.file_name().to_str()?.parse().ok()?;
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // `pid (comm) state ppid ...`; comm may contain spaces.
        let (comm, rest) = stat.split_once(" (")?.1.rsplit_once(") ")?;
        let mut fields = rest.split_whitespace();
        let state = fields.next()?;
        let ppid: u32 = fields.next()?.parse().ok()?;
        (ppid == parent && comm == name && state != "Z").then_some(pid)
    })
}

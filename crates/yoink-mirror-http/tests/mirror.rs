//! The HTTP backend behind the real API, against fake upstream and target
//! caches.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::StreamExt;
use nfi_auth::server::{Authenticator, Expr};
use nfi_auth::testing::MockIdp;
use nfi_interchange::nixbase32;
use sha2::Digest;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use yoink_api::client::Client;
use yoink_api::{Event, LogLevel, Outcome, Purl};
use yoink_mirror_http::narinfo::Signer;
use yoink_mirror_http::{Config, Credential, Endpoint, HttpMirror, NarLayout};
use yoink_server::{Auth, api};

#[derive(Default)]
struct Cache {
    files: Mutex<BTreeMap<String, Bytes>>,
    /// Required `Authorization` header value, if any.
    auth: Option<String>,
    /// Reject paths containing `/` (like GitLab generic packages).
    flat_only: bool,
    /// Fail this many PUTs with 503 first.
    fail_puts: AtomicUsize,
    gets: Mutex<HashMap<String, usize>>,
}

impl Cache {
    fn file(&self, p: &str) -> Option<Bytes> {
        self.files.lock().unwrap().get(p).cloned()
    }
    fn gets(&self, p: &str) -> usize {
        self.gets.lock().unwrap().get(p).copied().unwrap_or(0)
    }
}

async fn serve_cache(cache: Arc<Cache>, prefix: &str) -> String {
    async fn get_f(State(c): State<Arc<Cache>>, Path(p): Path<String>, h: HeaderMap) -> Response {
        if let Some(a) = &c.auth
            && h.get("authorization").and_then(|v| v.to_str().ok()) != Some(a)
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        *c.gets.lock().unwrap().entry(p.clone()).or_default() += 1;
        match c.file(&p) {
            Some(b) => b.into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }
    async fn put_f(
        State(c): State<Arc<Cache>>,
        Path(p): Path<String>,
        h: HeaderMap,
        body: Bytes,
    ) -> Response {
        if let Some(a) = &c.auth
            && h.get("authorization").and_then(|v| v.to_str().ok()) != Some(a)
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        if c.flat_only && p.contains('/') {
            return StatusCode::BAD_REQUEST.into_response();
        }
        if c.fail_puts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        c.files.lock().unwrap().insert(p, body);
        StatusCode::CREATED.into_response()
    }
    let inner = Router::new()
        .route("/{*path}", get(get_f).put(put_f))
        .with_state(cache);
    let app = if prefix.is_empty() {
        inner
    } else {
        Router::new().nest(prefix, inner)
    };
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}{prefix}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

fn hash(n: u8) -> String {
    let mut b = [3u8; 20];
    b[0] = n;
    nixbase32::encode(&b)
}

fn path(n: u8, name: &str) -> String {
    format!("/nix/store/{}-{name}", hash(n))
}

/// Add a path with a (fake, xz-labelled) NAR to an upstream cache.
fn add_path(c: &Cache, n: u8, name: &str, nar: &[u8], refs: &[u8]) {
    let file_hash = nixbase32::encode(&sha2::Sha256::digest(nar));
    let refs: Vec<String> = refs.iter().map(|r| format!("{}-dep", hash(*r))).collect();
    let ni = format!(
        "StorePath: {}\nURL: nar/{file_hash}.nar.xz\nCompression: xz\nFileHash: sha256:{file_hash}\n\
         FileSize: {}\nNarHash: sha256:{file_hash}\nNarSize: {}\nReferences: {}\nSig: cache.nixos.org-1:c2ln\n",
        path(n, name),
        nar.len(),
        nar.len() * 3,
        refs.join(" ")
    );
    let mut f = c.files.lock().unwrap();
    f.insert(format!("{}.narinfo", hash(n)), Bytes::from(ni));
    f.insert(format!("nar/{file_hash}.nar.xz"), Bytes::copy_from_slice(nar));
}

struct Env {
    upstream: Arc<Cache>,
    target: Arc<Cache>,
    client: Client,
}

async fn start(target: Cache, prefix: &str, f: impl FnOnce(&mut Config)) -> Env {
    let upstream = Arc::new(Cache::default());
    let target = Arc::new(target);
    let up_url = serve_cache(upstream.clone(), "").await;
    let t_url = serve_cache(target.clone(), prefix).await;
    let mut cfg = Config {
        upstream: Endpoint::new(&up_url, Credential::None),
        target: Endpoint::new(&t_url, Credential::Bearer("tok".into())),
        layout: NarLayout::Nar,
        signer: None,
        concurrency: 4,
        retries: 2,
        retry_delay: Duration::from_millis(10),
        cache_info_priority: Some(30),
        timeout: Duration::from_secs(10),
    };
    f(&mut cfg);
    let mirror = HttpMirror::new(cfg).await.unwrap();
    let idp = MockIdp::start("yoink").await;
    let auth = Auth {
        authenticator: Authenticator::new(idp.settings()).unwrap(),
        request: Expr::new("true").unwrap(),
        purl: Expr::new("true").unwrap(),
    };
    let state = api::State::new(Arc::new(mirror), auth, 1000);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, api::router(state)).await.unwrap() });
    let client = Client::new(&url, Some(idp.token(serde_json::json!({"sub": "ci"})))).unwrap();
    Env {
        upstream,
        target,
        client,
    }
}

fn target_cache() -> Cache {
    Cache {
        auth: Some("Bearer tok".into()),
        ..Default::default()
    }
}

async fn pull(c: &Client, purls: &[String]) -> (HashMap<String, Outcome>, Vec<Event>) {
    let s = c.mirror(purls).await.unwrap();
    let evs: Vec<Event> = tokio::time::timeout(Duration::from_secs(20), s.map(|e| e.unwrap()).collect())
        .await
        .expect("stream ends");
    let r = evs
        .iter()
        .filter_map(|e| match e {
            Event::Result { purl, outcome } => Some((purl.clone(), *outcome)),
            _ => None,
        })
        .collect();
    (r, evs)
}

fn logs(evs: &[Event], p: &str, level: LogLevel) -> Vec<String> {
    evs.iter()
        .filter_map(|e| match e {
            Event::Log {
                purl,
                level: l,
                message,
            } if purl == p && *l == level => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn nix(p: &str) -> String {
    Purl::nix(p).to_string()
}

#[tokio::test]
async fn copies_paths() {
    let e = start(target_cache(), "", |_| {}).await;
    add_path(&e.upstream, 1, "hello-2.12", b"hello nar contents", &[1, 2]);
    add_path(&e.upstream, 2, "glibc-2.40", &vec![42u8; 300_000], &[2]);
    // nix-cache-info was created at startup.
    let info = e.target.file("nix-cache-info").unwrap();
    assert!(String::from_utf8_lossy(&info).contains("Priority: 30"));

    let a = nix(&path(1, "hello-2.12"));
    // A hash-only PURL.
    let b = nix(&format!("/nix/store/{}", hash(2)));
    let (r, evs) = pull(&e.client, &[a.clone(), b.clone()]).await;
    assert_eq!(r[&a], Outcome::Success, "{evs:#?}");
    assert_eq!(r[&b], Outcome::Success, "{evs:#?}");
    match &evs[0] {
        Event::Phases { phases } => {
            let ab: String = phases.iter().map(|p| p.abbrev).collect();
            assert_eq!(ab, "QCNTP");
        }
        e => panic!("{e:?}"),
    }
    // The phases are visited in order, and the transfer reports byte progress.
    let phases: Vec<&str> = evs
        .iter()
        .filter_map(|ev| match ev {
            Event::Status { purl, phase, .. } if *purl == b => Some(phase.as_str()),
            _ => None,
        })
        .collect();
    let mut dedup = phases.clone();
    dedup.dedup();
    // The job may have left the queue before the stream subscribed.
    let all = ["queued", "checking", "narinfo", "transfer", "publish"];
    assert!(dedup.len() >= 4 && all.ends_with(&dedup), "{dedup:?}");
    assert!(evs.iter().any(|ev| matches!(ev,
        Event::Status { purl, progress: Some(p), .. } if *purl == b && p.max == 300_000 && p.current == 300_000)));

    let ni = String::from_utf8(
        e.target
            .file(&format!("{}.narinfo", hash(1)).to_string())
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let up = String::from_utf8(e.upstream.file(&format!("{}.narinfo", hash(1))).unwrap().to_vec()).unwrap();
    assert_eq!(ni, up, "the narinfo is copied as is with the default layout");
    let url = ni.lines().find_map(|l| l.strip_prefix("URL: ")).unwrap();
    assert_eq!(
        e.target.file(url).unwrap(),
        Bytes::from_static(b"hello nar contents")
    );

    // Mirroring again finds the paths in the target and fetches nothing.
    let before = e.upstream.gets(&format!("{}.narinfo", hash(1)));
    let (r, evs) = pull(&e.client, std::slice::from_ref(&a)).await;
    assert_eq!(r[&a], Outcome::Success);
    assert_eq!(e.upstream.gets(&format!("{}.narinfo", hash(1))), before);
    assert_eq!(logs(&evs, &a, LogLevel::Info), ["already in the target cache"]);
}

#[tokio::test]
async fn failures() {
    let e = start(target_cache(), "", |_| {}).await;
    add_path(&e.upstream, 1, "hello", b"abc", &[]);
    // Corrupt the NAR: wrong bytes with the right length.
    let file_hash = nixbase32::encode(&sha2::Sha256::digest(b"abc"));
    e.upstream
        .files
        .lock()
        .unwrap()
        .insert(format!("nar/{file_hash}.nar.xz"), Bytes::from_static(b"abd"));
    let corrupt = nix(&path(1, "hello"));
    let missing = nix(&path(9, "nope"));
    let wrong_name = nix(&path(1, "goodbye"));
    let npm = "pkg:npm/left-pad@1.0.0".to_string();
    let (r, evs) = pull(
        &e.client,
        &[corrupt.clone(), missing.clone(), wrong_name.clone(), npm.clone()],
    )
    .await;
    for p in [&corrupt, &missing, &wrong_name, &npm] {
        assert_eq!(r[p], Outcome::Fatal, "{p}: {evs:#?}");
        assert_eq!(logs(&evs, p, LogLevel::Fatal).len(), 1, "{p}");
    }
    let f = &logs(&evs, &corrupt, LogLevel::Fatal)[0];
    assert!(
        f.contains("hash mismatch") && f.contains("gave up after 3 attempts"),
        "{f}"
    );
    assert_eq!(logs(&evs, &corrupt, LogLevel::Warning).len(), 2, "two retries");
    assert!(logs(&evs, &missing, LogLevel::Fatal)[0].contains("not in the upstream cache"));
    assert!(logs(&evs, &npm, LogLevel::Fatal)[0].contains("unsupported package type"));
    // Nothing was published for the corrupt path (an orphaned NAR is harmless).
    assert!(e.target.file(&format!("{}.narinfo", hash(1))).is_none());
}

#[tokio::test]
async fn transient_target_errors_are_retried() {
    let t = target_cache();
    let e = start(t, "", |_| {}).await;
    e.target.fail_puts.store(2, Ordering::SeqCst);
    add_path(&e.upstream, 1, "hello", b"abc", &[]);
    let p = nix(&path(1, "hello"));
    let (r, evs) = pull(&e.client, std::slice::from_ref(&p)).await;
    assert_eq!(r[&p], Outcome::Success, "{evs:#?}");
    assert_eq!(logs(&evs, &p, LogLevel::Warning).len(), 2);
    assert!(e.target.file(&format!("{}.narinfo", hash(1))).is_some());
}

#[tokio::test]
async fn unauthorised_target_fails_at_startup() {
    let upstream = Arc::new(Cache::default());
    let target = Arc::new(target_cache());
    let up = serve_cache(upstream, "").await;
    let t = serve_cache(target, "").await;
    let r = HttpMirror::new(Config {
        upstream: Endpoint::new(&up, Credential::None),
        target: Endpoint::new(&t, Credential::Bearer("wrong".into())),
        layout: NarLayout::Nar,
        signer: None,
        concurrency: 1,
        retries: 0,
        retry_delay: Duration::from_millis(1),
        cache_info_priority: Some(30),
        timeout: Duration::from_secs(5),
    })
    .await;
    assert!(r.err().unwrap().to_string().contains("401"));
}

#[tokio::test]
async fn flat_layout_and_signing() {
    use base64::Engine;
    let seed = [9u8; 32];
    let kp = aws_lc_rs::signature::Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
    use aws_lc_rs::signature::KeyPair;
    let mut sk = seed.to_vec();
    sk.extend_from_slice(kp.public_key().as_ref());
    let key = format!(
        "yoink-test-1:{}",
        base64::engine::general_purpose::STANDARD.encode(&sk)
    );
    let t = Cache {
        flat_only: true,
        auth: Some("Bearer tok".into()),
        ..Default::default()
    };
    let e = start(t, "/api/v4/projects/1/packages/generic/nix-cache/1", |c| {
        c.layout = NarLayout::Flat;
        c.signer = Some(Signer::parse(&key).unwrap());
        c.target.credential =
            Credential::Header("authorization".parse().unwrap(), "Bearer tok".parse().unwrap());
    })
    .await;
    add_path(&e.upstream, 1, "hello", b"flat nar", &[1]);
    let p = nix(&path(1, "hello"));
    let (r, evs) = pull(&e.client, std::slice::from_ref(&p)).await;
    assert_eq!(r[&p], Outcome::Success, "{evs:#?}");
    let ni = String::from_utf8(e.target.file(&format!("{}.narinfo", hash(1))).unwrap().to_vec()).unwrap();
    let url = ni.lines().find_map(|l| l.strip_prefix("URL: ")).unwrap();
    assert!(!url.contains('/'), "{url}");
    assert_eq!(e.target.file(url).unwrap(), Bytes::from_static(b"flat nar"));
    let sigs: Vec<&str> = ni.lines().filter_map(|l| l.strip_prefix("Sig: ")).collect();
    assert_eq!(sigs.len(), 2);
    assert_eq!(sigs[0], "cache.nixos.org-1:c2ln");
    let parsed = yoink_mirror_http::narinfo::Narinfo::parse(&ni).unwrap();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(sigs[1].strip_prefix("yoink-test-1:").unwrap())
        .unwrap();
    aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::ED25519, kp.public_key())
        .verify(parsed.fingerprint().unwrap().as_bytes(), &raw)
        .unwrap();
}

#[tokio::test]
async fn concurrent_requests_share_jobs() {
    let e = start(target_cache(), "", |c| c.concurrency = 1).await;
    let purls: Vec<String> = (1..=6)
        .map(|n| {
            add_path(&e.upstream, n, "p", &vec![n; 50_000], &[]);
            nix(&path(n, "p"))
        })
        .collect();
    let (a, b) = tokio::join!(pull(&e.client, &purls), pull(&e.client, &purls));
    for (r, _) in [a, b] {
        assert_eq!(r.len(), 6);
        assert!(r.values().all(|o| *o == Outcome::Success));
    }
    for n in 1..=6 {
        assert_eq!(
            e.upstream.gets(&format!("{}.narinfo", hash(n))),
            1,
            "path {n} fetched once"
        );
    }
}

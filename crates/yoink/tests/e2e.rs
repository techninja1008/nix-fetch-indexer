//! The `yoink` binary against the real service and HTTP backend, copying
//! between in-memory caches.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use nfi_auth::server::{Authenticator, Expr};
use nfi_auth::testing::MockIdp;
use nfi_interchange::nixbase32;
use sha2::Digest;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use yoink_api::Purl;
use yoink_mirror_http::{Config, Credential, Endpoint, HttpMirror, NarLayout};
use yoink_server::{Auth, api};

type Files = Arc<Mutex<BTreeMap<String, Bytes>>>;

async fn serve_cache(files: Files) -> String {
    async fn get_f(State(f): State<Files>, Path(p): Path<String>) -> Response {
        match f.lock().unwrap().get(&p) {
            Some(b) => b.clone().into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }
    async fn put_f(State(f): State<Files>, Path(p): Path<String>, body: Bytes) -> StatusCode {
        f.lock().unwrap().insert(p, body);
        StatusCode::CREATED
    }
    let app = Router::new()
        .route("/{*path}", get(get_f).put(put_f))
        .with_state(files);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

fn store_path(n: u8) -> String {
    let mut b = [5u8; 20];
    b[0] = n;
    format!("/nix/store/{}-pkg-{n}", nixbase32::encode(&b))
}

fn add(files: &Files, n: u8, nar: &[u8]) {
    let p = store_path(n);
    let hash = &p[11..43];
    let fh = nixbase32::encode(&sha2::Sha256::digest(nar));
    let ni = format!(
        "StorePath: {p}\nURL: nar/{fh}.nar\nCompression: none\nFileHash: sha256:{fh}\nFileSize: {}\n\
         NarHash: sha256:{fh}\nNarSize: {}\nReferences: \n",
        nar.len(),
        nar.len()
    );
    let mut f = files.lock().unwrap();
    f.insert(format!("{hash}.narinfo"), Bytes::from(ni));
    f.insert(format!("nar/{fh}.nar"), Bytes::copy_from_slice(nar));
}

struct Stack {
    url: String,
    idp: MockIdp,
    target: Files,
}

/// The service with the HTTP backend between two caches, the upstream one
/// holding paths 1 to `n`.
async fn stack(n: u8) -> Stack {
    let upstream: Files = Default::default();
    let target: Files = Default::default();
    for n in 1..=n {
        add(&upstream, n, &vec![n; 10_000 + n as usize * 1000]);
    }
    let mirror = HttpMirror::new(Config {
        upstream: Endpoint::new(&serve_cache(upstream.clone()).await, Credential::None),
        target: Endpoint::new(&serve_cache(target.clone()).await, Credential::None),
        layout: NarLayout::Nar,
        signer: None,
        concurrency: 8,
        retries: 1,
        retry_delay: Duration::from_millis(10),
        cache_info_priority: Some(30),
        timeout: Duration::from_secs(10),
    })
    .await
    .unwrap();
    let idp = MockIdp::start("yoink").await;
    let auth = Auth {
        authenticator: Authenticator::new(idp.settings()).unwrap(),
        request: Expr::new("true").unwrap(),
        purl: Expr::new("true").unwrap(),
    };
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let state = api::State::new(Arc::new(mirror), auth, 1000);
    tokio::spawn(async move { axum::serve(l, api::router(state)).await.unwrap() });
    Stack { url, idp, target }
}

fn narinfo_count(f: &Files) -> usize {
    f.lock()
        .unwrap()
        .keys()
        .filter(|k| k.ends_with(".narinfo"))
        .count()
}

#[tokio::test]
async fn pulls_into_the_target_cache() {
    let Stack { url, idp, target } = stack(40).await;
    let token = idp.token(serde_json::json!({"sub": "ci", "preferred_username": "ci"}));
    let run = |purls: Vec<String>| {
        let d = tempfile::tempdir().unwrap();
        let mut c = tokio::process::Command::new(env!("CARGO_BIN_EXE_yoink"));
        c.arg("--plain")
            .args(purls)
            .env("YOINK_URL", &url)
            .env("YOINK_TOKEN", &token)
            .env("NFI_TOKEN_CACHE", d.path().join("tokens"))
            .env("XDG_CONFIG_HOME", d.path())
            .env("HOME", d.path())
            .env_remove("NFI_URL")
            .stdin(std::process::Stdio::null());
        async move {
            let out = tokio::time::timeout(Duration::from_secs(60), c.output())
                .await
                .expect("yoink finished")
                .unwrap();
            drop(d);
            (
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        }
    };

    let mut purls: Vec<String> = (1..=40).map(|n| Purl::nix(&store_path(n)).to_string()).collect();
    let (code, err) = run(purls.clone()).await;
    assert_eq!(code, Some(0), "{err}");
    assert!(err.contains("40 of 40 paths mirrored, 0 failed"), "{err}");
    {
        let t = target.lock().unwrap();
        for n in 1..=40u8 {
            let p = store_path(n);
            assert!(t.contains_key(&format!("{}.narinfo", &p[11..43])), "{p}");
        }
        assert!(t.contains_key("nix-cache-info"));
    }

    // Already mirrored paths succeed again; a path missing upstream fails.
    purls.push(Purl::nix(&store_path(99)).to_string());
    let (code, err) = run(purls).await;
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("not in the upstream cache"), "{err}");
    assert!(err.contains("40 of 41 paths mirrored, 1 failed"), "{err}");
}

/// `yoink` on a pseudo-terminal with an empty token cache.
async fn yoink_tty(url: &str, dir: &std::path::Path, args: &[String]) -> nfi_auth::testing::pty::PtyOutput {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_yoink"));
    c.args(args)
        .env("YOINK_URL", url)
        .env("NFI_TOKEN_CACHE", dir.join("tokens"))
        .env("XDG_CONFIG_HOME", dir)
        .env("HOME", dir)
        .env("TERM", "xterm-256color")
        .env_remove("YOINK_TOKEN")
        .env_remove("YOINK_TOKEN_FILE")
        .env_remove("NFI_URL");
    tokio::task::spawn_blocking(move || nfi_auth::testing::pty::run(c, 120, 40, Duration::from_secs(60)))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn interactive_login_and_monitor() {
    let s = stack(60).await;
    let d = tempfile::tempdir().unwrap();
    let purls = |r: std::ops::RangeInclusive<u8>| -> Vec<String> {
        r.map(|n| Purl::nix(&store_path(n)).to_string()).collect()
    };

    // No token and a terminal: log in first, then pull (plain output).
    let mut args = vec!["--plain".to_string()];
    args.extend(purls(1..=20));
    let o = yoink_tty(&s.url, d.path(), &args).await;
    assert_eq!(o.code, Some(0), "{}", o.text);
    assert!(
        o.text.contains("To log in, open") && o.text.contains("CODE-0"),
        "{}",
        o.text
    );
    assert!(o.text.contains("20 of 20 paths mirrored, 0 failed"), "{}", o.text);
    assert_eq!((s.idp.device_logins(), narinfo_count(&s.target)), (1, 20));

    // The fullscreen monitor, with the cached login: it runs until every
    // path has a result, restores the terminal and prints the summary.
    let o = yoink_tty(&s.url, d.path(), &purls(1..=60)).await;
    assert_eq!(o.code, Some(0), "{}", o.text);
    assert!(!o.timed_out);
    assert!(!o.text.contains("To log in"), "{}", o.text);
    // Drawn only by the fullscreen monitor (its key hints), not plain mode.
    assert!(o.text.contains("enter path log"), "{}", o.text);
    assert!(
        !o.text.contains(": Transferring NAR"),
        "no plain lines: {}",
        o.text
    );
    assert!(o.text.contains("60 of 60 paths mirrored, 0 failed"), "{}", o.text);
    assert_eq!((s.idp.device_logins(), narinfo_count(&s.target)), (1, 60));

    // `whoami` and `logout` from the terminal.
    let o = yoink_tty(&s.url, d.path(), &["whoami".into()]).await;
    assert_eq!(o.code, Some(0), "{}", o.text);
    assert!(o.text.contains("alice"), "{}", o.text);
    let o = yoink_tty(&s.url, d.path(), &["logout".into()]).await;
    assert_eq!(o.code, Some(0), "{}", o.text);
    assert!(o.text.contains("logged out of"), "{}", o.text);
    // Logged out, `whoami` logs in again like `nfi whoami`.
    let o = yoink_tty(&s.url, d.path(), &["whoami".into()]).await;
    assert_eq!(o.code, Some(0), "{}", o.text);
    assert!(o.text.contains("To log in, open"), "{}", o.text);
    assert_eq!(s.idp.device_logins(), 2);
}

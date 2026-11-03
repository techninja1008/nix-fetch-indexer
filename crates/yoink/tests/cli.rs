//! Run the `yoink` binary in plain mode against a scripted fake service.

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use std::sync::{Arc, Mutex};
use yoink_api::{Event, LogLevel, MirrorRequest, Outcome, PhaseDef, Progress, Unit};
use yoink_purl::Purl;

type Script = dyn Fn(usize, &[String]) -> Vec<Event> + Send + Sync;

#[derive(Clone)]
struct Fake {
    requests: Arc<Mutex<Vec<Vec<String>>>>,
    script: Arc<Script>,
}

async fn mirror(
    State(f): State<Fake>,
    headers: HeaderMap,
    axum::Json(r): axum::Json<MirrorRequest>,
) -> Response {
    if headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) != Some("Bearer tok") {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error": "no"})),
        )
            .into_response();
    }
    let n = {
        let mut reqs = f.requests.lock().unwrap();
        reqs.push(r.purls.clone());
        reqs.len() - 1
    };
    let body: String = (f.script)(n, &r.purls)
        .iter()
        .map(|e| {
            format!(
                "event: {}\ndata: {}\n\n",
                e.name(),
                serde_json::to_string(e).unwrap()
            )
        })
        .collect();
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

async fn serve(script: impl Fn(usize, &[String]) -> Vec<Event> + Send + Sync + 'static) -> (String, Fake) {
    let f = Fake {
        requests: Default::default(),
        script: Arc::new(script),
    };
    let config = || async {
        axum::Json(
            serde_json::json!({"issuer": "http://127.0.0.1:1", "client_id": "yoink", "scopes": ["openid"]}),
        )
    };
    let app = Router::new()
        .route("/v1/mirror", post(mirror))
        .route(nfi_auth::CONFIG_PATH, axum::routing::get(config))
        .with_state(f.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, f)
}

fn purl(n: u8, name: &str) -> String {
    Purl::nix(&format!("/nix/store/{n}xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b-{name}")).to_string()
}

fn phases() -> Event {
    Event::Phases {
        phases: vec![
            PhaseDef::new("queued", "Queued", 'Q'),
            PhaseDef::new("transfer", "Transferring NAR", 'T'),
        ],
    }
}

fn status(p: &str, phase: &str) -> Event {
    Event::Status {
        purl: p.into(),
        phase: phase.into(),
        progress: Some(Progress {
            current: 5,
            max: 10,
            unit: Some(Unit::Bytes),
        }),
    }
}

fn result(p: &str, outcome: Outcome) -> Event {
    Event::Result {
        purl: p.into(),
        outcome,
    }
}

/// A stand-in for nix in `dir`: it appends its arguments (`ARGS ...`) and
/// stdin lines (`STDIN ...`) to `dir/nix.log`, answers `store info --json`
/// with `trusted` from $NIX_STUB_TRUSTED (default true), and exits with
/// $NIX_STUB_EXIT (default 0).
fn nix_stub(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let (stub, log) = (dir.join("nix"), dir.join("nix.log"));
    let script = format!(
        r#"#!/bin/sh
echo "ARGS $*" >> '{log}'
case "$*" in
  *"store info --json"*) echo "{{\"trusted\":${{NIX_STUB_TRUSTED:-true}}}}"; exit 0 ;;
esac
while read -r l; do echo "STDIN $l" >> '{log}'; done
exit ${{NIX_STUB_EXIT:-0}}
"#,
        log = log.display()
    );
    std::fs::write(&stub, script).unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    stub
}

/// What the nix stub in `dir` was asked.
fn nix_log(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("nix.log"))
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

/// `yoink --plain` with its home, config and cache in `dir` (never the
/// developer's), and nix replaced by [`nix_stub`].
fn command(
    dir: &std::path::Path,
    url: &str,
    token: Option<&str>,
    args: &[String],
) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(env!("CARGO_BIN_EXE_yoink"));
    c.arg("--plain")
        .args(args)
        .env("YOINK_URL", url)
        .env("NFI_TOKEN_CACHE", dir.join("tokens"))
        .env("XDG_CONFIG_HOME", dir)
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("HOME", dir)
        .env("NFI_NIX", nix_stub(dir))
        .env_remove("YOINK_CACHE_URL")
        .env_remove("YOINK_TOKEN")
        .env_remove("YOINK_TOKEN_FILE")
        .env_remove("NFI_URL")
        .env_remove("NFI_ALLOW_INSECURE")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(t) = token {
        c.env("YOINK_TOKEN", t);
    }
    c
}

async fn yoink(url: &str, token: Option<&str>, args: &[String]) -> (i32, String) {
    let d = tempfile::tempdir().unwrap();
    yoink_in(d.path(), url, token, args).await
}

async fn yoink_in(dir: &std::path::Path, url: &str, token: Option<&str>, args: &[String]) -> (i32, String) {
    let mut c = command(dir, url, token, args);
    let out = tokio::time::timeout(std::time::Duration::from_secs(60), c.output())
        .await
        .expect("yoink finished")
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[tokio::test]
async fn success_and_failure() {
    let (a, b) = (purl(0, "hello"), purl(1, "glibc"));
    let (url, _) = serve(|_, purls| {
        let mut ev = vec![phases()];
        for p in purls {
            ev.push(status(p, "queued"));
            ev.push(status(p, "transfer"));
            if p.contains("glibc") {
                ev.push(Event::Log {
                    purl: p.clone(),
                    level: LogLevel::Fatal,
                    message: "upstream has no narinfo".into(),
                    id: None,
                });
                ev.push(result(p, Outcome::Fatal));
            } else {
                ev.push(result(p, Outcome::Success));
            }
        }
        ev
    })
    .await;

    let (code, err) = yoink(&url, Some("tok"), std::slice::from_ref(&a)).await;
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("hello: Transferring NAR") && err.contains("hello: done"),
        "{err}"
    );
    assert!(err.contains("1 of 1 paths mirrored, 0 failed"), "{err}");

    let (code, err) = yoink(&url, Some("tok"), &[a.clone(), b.clone(), a.clone()]).await;
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("glibc: fatal: upstream has no narinfo"), "{err}");
    assert!(err.contains("1 of 2 paths mirrored, 1 failed"), "{err}");
}

#[tokio::test]
async fn resumes_dropped_stream() {
    let (a, b, c) = (purl(0, "a"), purl(1, "b"), purl(2, "c"));
    let (url, fake) = serve(|n, purls| {
        let mut ev = vec![phases()];
        match n {
            // The first stream ends after one result.
            0 => {
                ev.push(status(&purls[0], "transfer"));
                ev.push(result(&purls[0], Outcome::Success));
                ev.push(status(&purls[1], "transfer"));
            }
            _ => ev.extend(purls.iter().map(|p| result(p, Outcome::Success))),
        }
        ev
    })
    .await;
    let (code, err) = yoink(&url, Some("tok"), &[a.clone(), b.clone(), c.clone()]).await;
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("reconnecting"), "{err}");
    let reqs = fake.requests.lock().unwrap().clone();
    assert_eq!(reqs, vec![vec![a, b.clone(), c.clone()], vec![b, c]]);
}

/// After a reconnect the service sends a job's logs again; they are shown
/// once.
#[tokio::test]
async fn resumed_logs_are_not_repeated() {
    let a = purl(0, "a");
    let (url, _) = serve(|n, purls| {
        let p = &purls[0];
        let log = |line: u64| Event::Log {
            purl: p.clone(),
            level: LogLevel::Warning,
            message: format!("line {line}"),
            id: Some(yoink_api::LogId { job: 4, line }),
        };
        let mut ev = vec![phases(), log(0), log(1), status(p, "transfer")];
        if n > 0 {
            ev.extend([log(2), result(p, Outcome::Success)]);
        }
        ev
    })
    .await;
    let (code, err) = yoink(&url, Some("tok"), &["-v".into(), a]).await;
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("reconnecting"), "{err}");
    for l in ["a: warning: line 0", "a: warning: line 1", "a: warning: line 2"] {
        assert_eq!(err.matches(l).count(), 1, "{l}: {err}");
    }
    // The repeated status isn't a new phase either.
    assert_eq!(err.matches("a: Transferring NAR").count(), 1, "{err}");
}

/// Ctrl-C in plain mode detaches: the summary is printed, Nix's lookups of
/// the paths already mirrored are refreshed (but nothing is downloaded, even
/// with --download), and the exit status is 130.
#[tokio::test]
async fn interrupt_detaches() {
    let (a, b) = (purl(0, "a"), purl(1, "b"));
    // `a` succeeds; `b` never finishes, and each stream ends early, so yoink
    // keeps reconnecting.
    let (url, fake) = serve(|_, purls| {
        let mut ev = vec![phases()];
        for p in purls {
            ev.push(match p.contains("-a") {
                true => result(p, Outcome::Success),
                false => status(p, "transfer"),
            });
        }
        ev
    })
    .await;
    let d = tempfile::tempdir().unwrap();
    let args = [
        "--download".into(),
        "--cache-url".into(),
        "http://cache.example".into(),
        a.clone(),
        b.clone(),
    ];
    let mut c = command(d.path(), &url, Some("tok"), &args);
    c.stderr(std::process::Stdio::piped());
    let child = c.spawn().unwrap();
    // Wait until it is monitoring (its first request was answered).
    for _ in 0..500 {
        if !fake.requests.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let pid = child.id().unwrap();
    // The shell's kill builtin: no kill binary needed (e.g. in the Nix sandbox).
    let st = std::process::Command::new("sh")
        .args(["-c", &format!("kill -INT {pid}")])
        .status()
        .unwrap();
    assert!(st.success());
    let out = tokio::time::timeout(std::time::Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("yoink exits on SIGINT")
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(130), "{err}");
    assert!(
        err.contains("1 of 2 paths mirrored, 0 failed, 1 unfinished"),
        "{err}"
    );
    assert!(err.contains("detached; the service keeps mirroring"), "{err}");
    assert_eq!(nix_log(d.path()), refresh_log(&[&a]), "{err}");
}

/// The stub's log for a trusted refresh of the store paths of `purls`.
fn refresh_log(purls: &[&str]) -> Vec<String> {
    let mut l = vec![
        "ARGS --extra-experimental-features nix-command store info --json".to_string(),
        "ARGS --extra-experimental-features nix-command build --dry-run --option narinfo-cache-negative-ttl 0 --stdin"
            .to_string(),
    ];
    l.extend(purls.iter().map(|p| format!("STDIN {}", store_path(p))));
    l
}

fn store_path(purl: &str) -> String {
    Purl::parse(purl).unwrap().nix_target().unwrap().path()
}

/// After mirroring, Nix is made to see the mirrored nix paths: a trusted
/// user's cached misses are refreshed, an untrusted one is told what to do,
/// and `--download` copies them into the local store instead.
#[tokio::test]
async fn nix_after_mirroring() {
    let (a, b, c) = (purl(0, "hello"), purl(1, "glibc"), purl(2, "zlib"));
    let npm = "pkg:npm/left-pad@1.0.0".to_string();
    let (url, fake) = serve(|_, purls| {
        let mut ev = vec![phases()];
        for p in purls {
            let ok = !p.contains("glibc");
            ev.push(result(p, if ok { Outcome::Success } else { Outcome::Fatal }));
        }
        ev
    })
    .await;
    let all = [a.clone(), b.clone(), c.clone(), npm.clone()];

    // Trusted: a dry-run build with the negative TTL at 0, of exactly the
    // nix paths that succeeded (on stdin).
    let d = tempfile::tempdir().unwrap();
    let (code, err) = yoink_in(d.path(), &url, Some("tok"), &all).await;
    assert_eq!(code, 1, "{err}");
    assert_eq!(nix_log(d.path()), refresh_log(&[&a, &c]), "{err}");
    assert!(!err.contains("trusted-users"), "{err}");

    // A failing refresh is only a warning.
    let d = tempfile::tempdir().unwrap();
    let mut cmd = command(d.path(), &url, Some("tok"), std::slice::from_ref(&a));
    let out = cmd.env("NIX_STUB_EXIT", "1").output().await.unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("warning: refreshing Nix's cached lookups"), "{err}");

    // Untrusted: no build, but guidance.
    let d = tempfile::tempdir().unwrap();
    let mut cmd = command(d.path(), &url, Some("tok"), &all);
    let out = cmd.env("NIX_STUB_TRUSTED", "false").output().await.unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert_eq!(nix_log(d.path()).len(), 1, "only the trust check: {err}");
    assert!(
        err.contains("trusted-users")
            && err.contains("narinfo-cache-negative-ttl")
            && err.contains("--download"),
        "{err}"
    );

    // --download: nix copy from the cache, and no dry-run.
    let d = tempfile::tempdir().unwrap();
    let mut args = vec![
        "--download".to_string(),
        "--cache-url".into(),
        "http://cache.example".into(),
    ];
    args.extend([a.clone(), c.clone()]);
    let (code, err) = yoink_in(d.path(), &url, Some("tok"), &args).await;
    assert_eq!(code, 0, "{err}");
    let mut want = vec![
        "ARGS --extra-experimental-features nix-command copy --from http://cache.example --option \
         narinfo-cache-negative-ttl 0 --stdin"
            .to_string(),
    ];
    want.extend([&a, &c].iter().map(|p| format!("STDIN {}", store_path(p))));
    assert_eq!(nix_log(d.path()), want, "{err}");

    // A failed download fails the command, after the summary.
    let d = tempfile::tempdir().unwrap();
    let mut cmd = command(d.path(), &url, Some("tok"), &args);
    let out = cmd.env("NIX_STUB_EXIT", "1").output().await.unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    let (sum, fail) = (
        err.find("2 of 2 paths mirrored").unwrap(),
        err.find("error: nix copy --from http://cache.example failed")
            .unwrap(),
    );
    assert!(sum < fail && err.contains("require-sigs"), "{err}");

    // No nix paths mirrored: nix isn't run.
    let d = tempfile::tempdir().unwrap();
    let (code, err) = yoink_in(d.path(), &url, Some("tok"), &[npm.clone(), b.clone()]).await;
    assert_eq!(code, 1, "{err}");
    assert!(nix_log(d.path()).is_empty(), "{err}");

    // --download without a cache URL is refused before any request.
    let before = fake.requests.lock().unwrap().len();
    let (code, err) = yoink(&url, Some("tok"), &["--download".into(), a.clone()]).await;
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("--download needs the cache to download from"),
        "{err}"
    );
    assert_eq!(fake.requests.lock().unwrap().len(), before);
}

#[tokio::test]
async fn authentication() {
    let a = purl(0, "a");
    let (url, fake) = serve(|_, _| vec![]).await;
    let (code, err) = yoink(&url, None, std::slice::from_ref(&a)).await;
    assert_eq!(code, 1);
    // Known before any request (none is sent: its 401 would show).
    assert!(
        err.contains("authentication required: run `yoink login`") && !err.contains("401"),
        "{err}"
    );
    let (code, err) = yoink(&url, Some("wrong"), std::slice::from_ref(&a)).await;
    assert_eq!(code, 1);
    assert!(
        err.contains("401") && err.contains("the token was rejected"),
        "{err}"
    );
    assert!(fake.requests.lock().unwrap().is_empty());

    let (code, err) = yoink(&url, Some("tok"), &["hello@1.0".into()]).await;
    assert_eq!(code, 1);
    assert!(err.contains("not yet supported"), "{err}");
}

/// A token is not sent over plain HTTP to a host other than loopback unless
/// allowed: the command fails before any request (192.0.2.1 is unroutable).
#[tokio::test]
async fn insecure_url_is_refused() {
    let (code, err) = yoink("http://192.0.2.1", Some("tok"), &[purl(0, "a")]).await;
    assert_eq!(code, 1);
    assert!(
        err.contains("refusing to use credentials with http://192.0.2.1")
            && err.contains("--allow-insecure or set NFI_ALLOW_INSECURE=1"),
        "{err}"
    );
}

/// A login the NFI service needs is known before nix evaluates anything:
/// without a terminal the command fails then, having sent NFI no request
/// but for its settings.
#[tokio::test(flavor = "multi_thread")]
async fn nfi_login_needed_before_evaluating() {
    use std::os::unix::fs::PermissionsExt;
    let idp = nfi_test_support::MockIdp::start("nfi").await;
    let nfi = nfi_test_support::Impostor::start(&idp).await;
    let (url, fake) = serve(|_, _| vec![]).await;
    let d = tempfile::tempdir().unwrap();
    let (nix, ran) = (d.path().join("nix"), d.path().join("ran"));
    std::fs::write(&nix, format!("#!/bin/sh\ntouch '{}'\nexit 1\n", ran.display())).unwrap();
    std::fs::set_permissions(&nix, std::fs::Permissions::from_mode(0o755)).unwrap();
    let args = [
        "--nix".to_string(),
        nix.display().to_string(),
        "--nfi-url".into(),
        nfi.url().into(),
        "nixpkgs#hello".into(),
    ];
    let (code, err) = yoink(&url, Some("tok"), &args).await;
    assert_eq!(code, 1);
    assert!(
        err.contains("authentication required: run `yoink login` or set NFI_TOKEN"),
        "{err}"
    );
    assert!(!ran.exists());
    assert_eq!((idp.config_fetches(), nfi.requests()), (1, 0));
    assert!(fake.requests.lock().unwrap().is_empty());
}

/// `whoami` checks whichever service URLs are configured, reporting errors
/// on stderr.
#[tokio::test(flavor = "multi_thread")]
async fn whoami_checks_the_configured_urls() {
    let (url, _) = serve(|_, _| vec![]).await;
    let d = tempfile::tempdir().unwrap();
    let whoami = |env: &[(&str, &str)]| {
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_yoink"));
        c.arg("whoami")
            .env("NFI_TOKEN_CACHE", d.path().join("tokens"))
            .env("XDG_CONFIG_HOME", d.path())
            .env_remove("YOINK_URL")
            .env_remove("NFI_URL")
            .env_remove("YOINK_TOKEN")
            .env_remove("NFI_TOKEN")
            .envs(env.iter().copied());
        let o = tokio::task::block_in_place(|| c.output().unwrap());
        (
            o.status.code(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        )
    };
    let (code, out, err) = whoami(&[]);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(err.contains("no yoink service URL"), "{err}");
    // Only the NFI URL (the fake has no whoami: no login).
    let (code, out, err) = whoami(&[("NFI_URL", &url)]);
    assert_eq!(
        (code, out.trim()),
        (Some(0), format!("{url}: does not use login").as_str()),
        "{err}"
    );
    let (code, out, err) = whoami(&[("NFI_URL", &url), ("YOINK_URL", "http://127.0.0.1:9")]);
    assert_eq!(code, Some(1));
    assert_eq!(out.trim(), format!("{url}: does not use login"));
    assert!(
        err.starts_with("error: http://127.0.0.1:9: connecting to"),
        "{err}"
    );
}

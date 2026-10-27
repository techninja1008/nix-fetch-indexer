//! Run the `yoink` binary in plain mode against a scripted fake service.

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use std::sync::{Arc, Mutex};
use yoink_api::{Event, LogLevel, MirrorRequest, Outcome, PhaseDef, Progress, Purl, Unit};

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
        .map(|e| format!("event: {}\ndata: {}\n\n", e.name(), e.data()))
        .collect();
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

async fn serve(script: impl Fn(usize, &[String]) -> Vec<Event> + Send + Sync + 'static) -> (String, Fake) {
    let f = Fake {
        requests: Default::default(),
        script: Arc::new(script),
    };
    let app = Router::new()
        .route("/v1/mirror", post(mirror))
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

async fn yoink(url: &str, token: Option<&str>, args: &[String]) -> (i32, String) {
    let d = tempfile::tempdir().unwrap();
    let mut c = tokio::process::Command::new(env!("CARGO_BIN_EXE_yoink"));
    c.arg("--plain")
        .args(args)
        .env("YOINK_URL", url)
        .env("NFI_TOKEN_CACHE", d.path().join("tokens"))
        .env("XDG_CONFIG_HOME", d.path())
        .env("HOME", d.path())
        .env_remove("YOINK_TOKEN")
        .env_remove("YOINK_TOKEN_FILE")
        .env_remove("NFI_URL")
        .stdin(std::process::Stdio::null());
    if let Some(t) = token {
        c.env("YOINK_TOKEN", t);
    }
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

#[tokio::test]
async fn authentication() {
    let a = purl(0, "a");
    let (url, fake) = serve(|_, _| vec![]).await;
    let (code, err) = yoink(&url, None, std::slice::from_ref(&a)).await;
    assert_eq!(code, 1);
    assert!(
        err.contains("requires authentication") && err.contains("yoink login"),
        "{err}"
    );
    let (code, err) = yoink(&url, Some("wrong"), std::slice::from_ref(&a)).await;
    assert_eq!(code, 1);
    assert!(err.contains("401") && err.contains("yoink login"), "{err}");
    assert!(fake.requests.lock().unwrap().is_empty());

    let (code, err) = yoink(&url, Some("tok"), &["hello@1.0".into()]).await;
    assert_eq!(code, 1);
    assert!(err.contains("not yet supported"), "{err}");
}

//! The per-PURL log of mirror requests. In its own test binary: tracing
//! caches per-callsite interest, so tests logging concurrently without a
//! subscriber could hide these lines.

use futures::StreamExt;
use nfi_auth_server::{Expr, OidcSettings};
use nfi_test_support::MockIdp;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use yoink::client::Client;
use yoink_purl::Purl;
use yoink_server::{Auth, Job, JobTable, Mirror, api};

/// Mirrors `pkg:fake/*`; names containing "bad" fail, and those containing
/// "slow" report progress until `release`.
#[derive(Default)]
struct Fake {
    jobs: JobTable<String>,
    release: Arc<tokio::sync::Notify>,
}

impl Mirror for Fake {
    fn phases(&self) -> Vec<yoink_api::PhaseDef> {
        vec![]
    }

    fn mirror(&self, purl: &Purl) -> anyhow::Result<Job> {
        if purl.ty != "fake" {
            anyhow::bail!("unsupported package type {}", purl.ty);
        }
        let name = purl.name.clone();
        let release = self.release.clone();
        Ok(self.jobs.get_or_start(name.clone(), "q", move |r| async move {
            if name.contains("slow") {
                let released = release.notified();
                tokio::pin!(released);
                for i in 0.. {
                    tokio::select! {
                        _ = &mut released => break,
                        _ = tokio::time::sleep(Duration::from_millis(20)) => r.progress(i, u64::MAX, None),
                    }
                }
            }
            if name.contains("bad") {
                anyhow::bail!("{name} is bad");
            }
            Ok(())
        }))
    }
}

/// Collects formatted log output.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Log {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn purls_are_logged_by_user() {
    let log = Log::default();
    let w = log.clone();
    let sub = tracing_subscriber::fmt()
        .with_writer(move || w.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    // The test runtime is single-threaded, so the server logs here too.
    let _g = tracing::subscriber::set_default(sub);

    let idp = MockIdp::start("yoink").await;
    let auth = Auth::new(
        Arc::new(
            nfi_auth_server::Authenticator::new(OidcSettings::new(idp.issuer(), idp.client_id())).unwrap(),
        ),
        Expr::new("true").unwrap(),
        Expr::new("!(purl.name.startsWith('secret'))").unwrap(),
    );
    let fake = Arc::new(Fake::default());
    let state = api::State::new(fake.clone(), auth, 10);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, api::router(state)).await.unwrap() });

    let token = idp.token(json!({"sub": "a", "preferred_username": "alice"}));
    let c = Client::new(&url, Some(token)).unwrap();
    let purls: Vec<String> = [
        "pkg:fake/ok",
        "pkg:fake/bad",
        "pkg:fake/secret1",
        "pkg:other/x",
        "not-a-purl",
    ]
    .map(String::from)
    .to_vec();
    let s = c.mirror(&purls).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), s.collect::<Vec<_>>())
        .await
        .expect("stream ends");

    // A client gone before its PURL finishes: the outcome is still logged.
    let mut s = c.mirror(&["pkg:fake/slow".to_string()]).await.unwrap();
    while let Some(e) = s.next().await {
        if e.unwrap().purl() == Some("pkg:fake/slow") {
            break;
        }
    }
    drop(s);
    // Progress keeps being written until the server notices the client is
    // gone and drops the stream.
    tokio::time::sleep(Duration::from_millis(500)).await;
    fake.release.notify_one();
    let slow_done = || {
        String::from_utf8_lossy(&log.0.lock().unwrap())
            .contains("PURL done purl=pkg:fake/slow outcome=Success")
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while !slow_done() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("outcome logged after the client left");

    let out = String::from_utf8_lossy(&log.0.lock().unwrap()).into_owned();
    let lines: Vec<&str> = out.lines().collect();
    let find = |pat: &[&str]| {
        assert!(
            lines.iter().any(|l| pat.iter().all(|p| l.contains(p))),
            "no line with {pat:?} in {lines:#?}"
        )
    };
    let user = "user=alice";
    find(&[user, "mirroring PURL", "purl=pkg:fake/ok"]);
    find(&[user, "PURL done", "purl=pkg:fake/ok", "outcome=Success"]);
    find(&[user, "PURL done", "purl=pkg:fake/bad", "outcome=Fatal"]);
    find(&[user, "PURL not authorised", "purl=pkg:fake/secret1"]);
    find(&[
        user,
        "PURL not mirrored",
        "purl=pkg:other/x",
        "unsupported package type other",
    ]);
    find(&[user, "invalid PURL", "purl=not-a-purl"]);
    find(&[user, "PURL done", "purl=pkg:fake/slow"]);
}

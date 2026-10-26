use futures::StreamExt;
use nfi_auth::server::Expr;
use nfi_auth::testing::MockIdp;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use yoink_api::client::{Client, HttpError};
use yoink_api::{Event, LogLevel, Outcome, PhaseDef, Purl, Unit};
use yoink_server::{Auth, Job, JobTable, Mirror, api};

/// A backend whose jobs wait for a release, then succeed unless their name
/// contains "bad".
#[derive(Default)]
struct Fake {
    jobs: JobTable<String>,
    started: Arc<AtomicUsize>,
    gates: Arc<Mutex<HashMap<String, tokio::sync::watch::Sender<bool>>>>,
}

impl Fake {
    fn gate(&self, name: &str) -> tokio::sync::watch::Sender<bool> {
        self.gates
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_insert_with(|| tokio::sync::watch::channel(false).0)
            .clone()
    }
}

impl Mirror for Fake {
    fn phases(&self) -> Vec<PhaseDef> {
        vec![
            PhaseDef::new("q", "Queued", 'Q'),
            PhaseDef::new("t", "Transferring", 'T'),
        ]
    }

    fn mirror(&self, purl: &Purl) -> anyhow::Result<Job> {
        if purl.ty != "fake" {
            anyhow::bail!("unsupported package type {}", purl.ty);
        }
        let name = purl.name.clone();
        let started = self.started.clone();
        let mut gate = self.gate(&name).subscribe();
        Ok(self.jobs.get_or_start(name.clone(), "q", move |r| async move {
            started.fetch_add(1, Ordering::SeqCst);
            gate.wait_for(|open| *open).await?;
            r.phase("t");
            r.progress(50, 100, Some(Unit::Bytes));
            r.warn("halfway");
            if name.contains("bad") {
                anyhow::bail!("{name} is bad");
            }
            r.progress(100, 100, Some(Unit::Bytes));
            Ok(())
        }))
    }
}

struct Env {
    idp: MockIdp,
    fake: Arc<Fake>,
    url: String,
}

async fn start(request: &str, purl: &str) -> Env {
    let idp = MockIdp::start("yoink").await;
    let fake = Arc::new(Fake::default());
    let auth = Auth {
        authenticator: nfi_auth::server::Authenticator::new(idp.settings()).unwrap(),
        request: Expr::new(request).unwrap(),
        purl: Expr::new(purl).unwrap(),
    };
    let state = api::State::new(fake.clone(), auth, 10);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, api::router(state)).await.unwrap() });
    Env { idp, fake, url }
}

fn token(e: &Env, user: &str) -> String {
    e.idp
        .token(json!({"sub": user, "preferred_username": user, "groups": ["dev"]}))
}

async fn collect(c: &Client, purls: &[&str]) -> Vec<Event> {
    let purls: Vec<String> = purls.iter().map(|s| s.to_string()).collect();
    let s = c.mirror(&purls).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), s.map(|e| e.unwrap()).collect())
        .await
        .expect("stream ends")
}

fn results(evs: &[Event]) -> HashMap<String, Outcome> {
    evs.iter()
        .filter_map(|e| match e {
            Event::Result { purl, outcome } => Some((purl.clone(), *outcome)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn mirror_stream() {
    let e = start("true", "!(purl.name.startsWith('secret'))").await;
    e.fake.gate("ok").send_replace(true);
    e.fake.gate("bad").send_replace(true);
    let c = Client::new(&e.url, Some(token(&e, "alice"))).unwrap();
    let evs = collect(
        &c,
        &[
            "pkg:fake/ok",
            "pkg:fake/bad",
            "pkg:other/x",
            "not-a-purl",
            "pkg:fake/secret1",
            "pkg:fake/ok",
        ],
    )
    .await;
    assert!(matches!(&evs[0], Event::Phases { phases } if phases.len() == 2));
    let r = results(&evs);
    assert_eq!(r.len(), 5, "{r:?}");
    assert_eq!(r["pkg:fake/ok"], Outcome::Success);
    for p in ["pkg:fake/bad", "pkg:other/x", "not-a-purl", "pkg:fake/secret1"] {
        assert_eq!(r[p], Outcome::Fatal, "{p}");
        assert!(
            evs.iter()
                .any(|ev| matches!(ev, Event::Log { purl, level: LogLevel::Fatal, .. } if purl == p)),
            "{p} has a fatal log"
        );
    }
    // Each PURL's result is its last event.
    for p in r.keys() {
        let last = evs.iter().rposition(|ev| ev.purl() == Some(p)).unwrap();
        assert!(matches!(&evs[last], Event::Result { .. }));
    }
    assert!(evs.iter().any(|ev| matches!(ev,
        Event::Status { purl, phase, progress: Some(_) } if purl == "pkg:fake/ok" && phase == "t")));
    assert!(evs.iter().any(|ev| matches!(ev,
        Event::Log { purl, level: LogLevel::Warning, message } if purl == "pkg:fake/bad" && message == "halfway")));
    let who: nfi_auth::Whoami = c.get_json(nfi_auth::WHOAMI_PATH).await.unwrap();
    assert_eq!(who.username, "alice");
    let cfg: nfi_auth::AuthConfig = Client::new(&e.url, None)
        .unwrap()
        .get_json(nfi_auth::CONFIG_PATH)
        .await
        .unwrap();
    assert_eq!(cfg.client_id, "yoink");
}

fn status(e: anyhow::Error) -> u16 {
    e.downcast_ref::<HttpError>().expect("an HTTP error").status
}

#[tokio::test]
async fn auth_required() {
    let e = start("'dev' in claims.groups && user.username != 'mallory'", "true").await;
    let purls = vec!["pkg:fake/x".to_string()];
    let anon = Client::new(&e.url, None).unwrap();
    assert_eq!(status(anon.mirror(&purls).await.err().unwrap()), 401);
    let bad = Client::new(&e.url, Some("garbage".into())).unwrap();
    assert_eq!(status(bad.mirror(&purls).await.err().unwrap()), 401);
    let expired = e.idp.token(json!({"sub": "a", "exp": 1000}));
    let c = Client::new(&e.url, Some(expired)).unwrap();
    assert_eq!(status(c.mirror(&purls).await.err().unwrap()), 401);
    let m = Client::new(&e.url, Some(token(&e, "mallory"))).unwrap();
    assert_eq!(status(m.mirror(&purls).await.err().unwrap()), 403);
    let nogroup = Client::new(&e.url, Some(e.idp.token(json!({"sub": "b"})))).unwrap();
    assert_eq!(status(nogroup.mirror(&purls).await.err().unwrap()), 403);
    let c = Client::new(&e.url, Some(token(&e, "alice"))).unwrap();
    let too_many: Vec<String> = (0..11).map(|i| format!("pkg:fake/{i}")).collect();
    assert_eq!(status(c.mirror(&too_many).await.err().unwrap()), 413);
    assert_eq!(status(c.mirror(&[]).await.err().unwrap()), 400);
}

#[tokio::test]
async fn jobs_are_shared_and_outlive_streams() {
    let e = start("true", "true").await;
    let c = Client::new(&e.url, Some(token(&e, "alice"))).unwrap();
    let purls = vec!["pkg:fake/slow".to_string()];

    // A stream dropped mid-job leaves the job running.
    let mut s = c.mirror(&purls).await.unwrap();
    assert!(matches!(s.next().await, Some(Ok(Event::Phases { .. }))));
    assert!(matches!(s.next().await, Some(Ok(Event::Status { .. }))));
    drop(s);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(e.fake.jobs.len(), 1);

    // Two concurrent requests attach to that one job.
    let mut a = c.mirror(&purls).await.unwrap();
    let mut b = c.mirror(&purls).await.unwrap();
    for s in [&mut a, &mut b] {
        assert!(matches!(s.next().await, Some(Ok(Event::Phases { .. }))));
        assert!(matches!(s.next().await, Some(Ok(Event::Status { .. }))));
    }
    e.fake.gate("slow").send_replace(true);
    for s in [a, b] {
        let evs: Vec<Event> = s.map(|e| e.unwrap()).collect().await;
        assert_eq!(results(&evs)["pkg:fake/slow"], Outcome::Success);
    }
    assert_eq!(e.fake.started.load(Ordering::SeqCst), 1);

    // Once finished, a new request starts a new job.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let evs = collect(&c, &["pkg:fake/slow"]).await;
    assert_eq!(results(&evs)["pkg:fake/slow"], Outcome::Success);
    assert_eq!(e.fake.started.load(Ordering::SeqCst), 2);
}

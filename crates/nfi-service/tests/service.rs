//! Integration tests against ephemeral Postgres and Meilisearch.

use nfi_api::*;
use nfi_interchange::*;
use serde_json::{Value, json};
use std::time::Duration;

use nfi_testenv::{A, bundle, h, path};

fn encode(b: &Bundle, format: Format) -> Vec<u8> {
    let mut v = Vec::new();
    codec::write(b, &mut v, format).unwrap();
    v
}

struct Client {
    http: reqwest::Client,
    svc: nfi_testenv::TestService,
    token: Option<String>,
}

impl Client {
    fn admin(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let r = self.http.request(method, format!("{}{path}", self.svc.admin_url));
        match &self.token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }

    async fn wait_job(&self, id: i64) -> JobInfo {
        for _ in 0..600 {
            let j: JobInfo = self
                .admin(reqwest::Method::GET, &format!("/admin/v1/jobs/{id}"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if j.status.is_finished() {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("job {id} did not finish");
    }

    async fn ingest_bytes(&self, data: Vec<u8>) -> JobInfo {
        let r = self
            .admin(reqwest::Method::POST, "/admin/v1/ingest")
            .body(data)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 202);
        let c: JobCreated = r.json().await.unwrap();
        let j = self.wait_job(c.id).await;
        assert_eq!(j.status, JobStatus::Succeeded, "{:?}", j.error);
        j
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> (u16, T) {
        let r = self
            .http
            .get(format!("{}{path}", self.svc.public_url))
            .send()
            .await
            .unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap())
    }

    async fn post<T: serde::de::DeserializeOwned>(&self, path: &str, body: &Value) -> (u16, T) {
        let r = self
            .http
            .post(format!("{}{path}", self.svc.public_url))
            .json(body)
            .send()
            .await
            .unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap())
    }
}

async fn setup(token: Option<&str>) -> Option<Client> {
    let svc = nfi_testenv::start_service(token).await?;
    Some(Client {
        http: reqwest::Client::new(),
        svc,
        token: token.map(str::to_string),
    })
}

#[tokio::test]
async fn ingest_lookup_bulk_minimise_prune() {
    let Some(c) = setup(None).await else { return };
    let job = c
        .ingest_bytes(encode(&bundle(), Format::Binary(Compression::Zstd(3))))
        .await;
    let result = job.result.unwrap();
    assert_eq!(result["store_paths"], 6);
    assert_eq!(result["missing"], 1);

    // Releases with datasets.
    let rels: Vec<ReleaseInfo> = c
        .admin(reqwest::Method::GET, "/admin/v1/releases")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rels.len(), 2);
    let a = rels.iter().find(|r| r.commit == A).unwrap();
    assert_eq!(a.datasets, vec!["closure", "outpaths", "packages", "programs"]);
    assert_eq!(a.series, "25.05");

    // Direct lookup (with a commit prefix).
    let (s, p): (u16, PackageInfo) = c.get("/v1/packages/curl?commit=aaaaaaa").await;
    assert_eq!(s, 200);
    assert_eq!(p.version.as_deref(), Some("8.0"));
    assert_eq!(p.release.channel, "nixos-25.05");
    assert!(p.outputs["out"].cached);
    assert!(!p.outputs["dev"].cached);
    assert_eq!(p.programs, vec!["curl"]);
    let (s, _): (u16, Value) = c.get("/v1/packages/curl?commit=bbbb").await;
    assert_eq!(s, 404);
    let (s, _): (u16, Value) = c.get("/v1/packages/curl?commit=cccc").await;
    assert_eq!(s, 404);

    // Bulk lookup.
    let (s, bulk): (u16, BulkResponse) = c
        .post(
            "/v1/lookup/bulk",
            &json!({"items": [
                {"attr": "hello", "commit": A},
                {"attr": "curl", "commit": A},
                {"attr": "curl", "commit": A, "outputs": ["*"]},
                {"attr": "missing", "commit": A},
                {"attr": "nooutpaths", "commit": A},
                {"attr": "hello", "commit": "dddd"},
            ]}),
        )
        .await;
    assert_eq!(s, 200);
    let st: Vec<ItemStatus> = bulk.items.iter().map(|i| i.status).collect();
    assert_eq!(
        st,
        vec![
            ItemStatus::Ok,
            ItemStatus::Ok,
            ItemStatus::NotCached,
            ItemStatus::UnknownAttr,
            ItemStatus::NoOutpaths,
            ItemStatus::UnknownCommit
        ]
    );
    let mut expect = vec![
        path(1, "glibc-2.40"),
        path(2, "hello-2.12"),
        path(3, "curl-8.0"),
        path(4, "openssl-3.0"),
    ];
    expect.sort();
    assert_eq!(bulk.closure, expect);
    assert_eq!(bulk.nar_size, 10_000);

    // Minimise with a raw (v4) `nix derivation show` document.
    let drv = |n: u8, name: &str| format!("{}-{name}.drv", h(100 + n));
    let graph = json!({"version": 4, "derivations": {
        drv(1, "app"): {"outputs": {"out": {"path": format!("{}-app", h(50))}},
            "inputs": {"drvs": {drv(2, "hello"): {"outputs": ["out"], "dynamicOutputs": {}}, drv(3, "mylib"): {"outputs": ["out"], "dynamicOutputs": {}}},
                       "srcs": [format!("{}-src.tar", h(51))]}},
        drv(2, "hello"): {"outputs": {"out": {"path": format!("{}-hello-2.12", h(2))}},
            "inputs": {"drvs": {drv(4, "glibc"): {"outputs": ["out"], "dynamicOutputs": {}}}, "srcs": []}},
        drv(3, "mylib"): {"outputs": {"out": {"path": format!("{}-mylib", h(52))}},
            "inputs": {"drvs": {drv(5, "openssl"): {"outputs": ["dev"], "dynamicOutputs": {}}, drv(4, "glibc"): {"outputs": ["out"], "dynamicOutputs": {}}}, "srcs": []}},
        drv(4, "glibc"): {"outputs": {"out": {"path": format!("{}-glibc-2.40", h(1))}}, "inputs": {"drvs": {}, "srcs": []}},
        drv(5, "openssl"): {"outputs": {"out": {"path": format!("{}-openssl-3.0", h(4))}, "dev": {"path": format!("{}-openssl-3.0-dev", h(5))}},
            "inputs": {"drvs": {}, "srcs": []}},
    }});
    let (s, m): (u16, MinimiseResponse) = c.post("/v1/minimise", &graph).await;
    assert_eq!(s, 200);
    assert_eq!(m.requested, vec![format!("/nix/store/{}", drv(1, "app"))]);
    assert_eq!(m.to_build.len(), 2);
    let mut roots = vec![path(2, "hello-2.12"), path(5, "openssl-3.0-dev")];
    roots.sort();
    assert_eq!(m.roots, roots);
    let mut cl = vec![
        path(1, "glibc-2.40"),
        path(2, "hello-2.12"),
        path(4, "openssl-3.0"),
        path(5, "openssl-3.0-dev"),
    ];
    cl.sort();
    assert_eq!(m.closure, cl);
    // The reduced form gives the same answer.
    let reduced = nfi_api::drv::ReducedGraph::from_json(&graph).unwrap();
    let (_, m2): (u16, MinimiseResponse) = c
        .post("/v1/minimise", &serde_json::to_value(&reduced).unwrap())
        .await;
    assert_eq!(m2.closure, m.closure);
    assert!(m.warnings.is_empty(), "{:?}", m.warnings);

    // stdenv anchor: an uncached stdenv means an unindexed nixpkgs revision.
    let stdenv_graph = |out: String| {
        json!({"version": 4, "derivations": {
            drv(9, "stdenv-linux"): {"outputs": {"out": {"path": out}}, "inputs": {"drvs": {}, "srcs": []}},
            drv(1, "app"): {"outputs": {"out": {"path": format!("{}-app", h(50))}},
                "inputs": {"drvs": {drv(9, "stdenv-linux"): {"outputs": ["out"], "dynamicOutputs": {}}}, "srcs": []}},
        }})
    };
    let (_, m): (u16, MinimiseResponse) = c
        .post("/v1/minimise", &stdenv_graph(format!("{}-stdenv-linux", h(77))))
        .await;
    assert_eq!(m.warnings.len(), 1, "{:?}", m.warnings);
    assert_eq!(m.warnings[0].code, "unindexed_nixpkgs");
    assert_eq!(
        m.warnings[0].paths,
        vec![format!("/nix/store/{}", drv(9, "stdenv-linux"))]
    );
    // A cached stdenv output (reusing an indexed path) raises no warning.
    let (_, m): (u16, MinimiseResponse) = c
        .post("/v1/minimise", &stdenv_graph(format!("{}-glibc-2.40", h(1))))
        .await;
    assert!(m.warnings.is_empty(), "{:?}", m.warnings);

    let (s, _): (u16, Value) = c.post("/v1/minimise", &json!({"nope": 1})).await;
    assert_eq!(s, 400);

    // Re-ingesting (as JSON this time) is idempotent.
    c.ingest_bytes(encode(&bundle(), Format::Json)).await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM store_path_releases")
        .fetch_one(&c.svc.state.pool)
        .await
        .unwrap();
    assert_eq!(n, 5 + 3);

    // A release name already indexed under a different commit is rejected.
    let mut conflicting = bundle();
    conflicting.releases[0].commit = "c".repeat(40);
    let r = c
        .admin(reqwest::Method::POST, "/admin/v1/ingest")
        .body(encode(&conflicting, Format::Json))
        .send()
        .await
        .unwrap();
    let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
    assert_eq!(j.status, JobStatus::Failed);
    assert!(j.error.unwrap().contains("already indexed with commit"));

    // Search (merged packages + programs).
    if c.svc.has_search {
        let (s, r): (u16, SearchResponse) = c.get("/v1/search?q=greeting").await;
        assert_eq!(s, 200);
        assert_eq!(r.hits[0].attr, "hello");
        assert_eq!(r.hits[0].programs, vec!["hello"]);
        let (_, r): (u16, SearchResponse) = c.get("/v1/search?program=curl").await;
        assert_eq!(
            r.hits.iter().map(|h| h.attr.as_str()).collect::<Vec<_>>(),
            vec!["curl"]
        );
        let (_, r): (u16, SearchResponse) = c.get("/v1/search?series=unstable").await;
        let mut attrs: Vec<_> = r.hits.iter().map(|h| h.attr.clone()).collect();
        attrs.sort();
        assert_eq!(attrs, vec!["hello", "orphanpkg"]);
        let (_, r): (u16, SearchResponse) = c.get(&format!("/v1/search?q=transfer&commit={A}")).await;
        assert_eq!(r.hits.len(), 1);
        let (_, r): (u16, SearchResponse) = c.get("/v1/search?q=orphan-tool").await;
        assert_eq!(r.hits[0].attr, "orphanpkg");
    }

    // Prune the unstable channel: its release and its exclusive paths go away.
    let r = c
        .admin(reqwest::Method::POST, "/admin/v1/prune")
        .json(&json!({"channels": ["nixos-unstable"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let id = r.json::<JobCreated>().await.unwrap().id;
    let j = c.wait_job(id).await;
    assert_eq!(j.status, JobStatus::Succeeded, "{:?}", j.error);
    assert_eq!(
        j.result.as_ref().unwrap()["releases"],
        json!(["nixos-26.11pre200.bbbbbbbbbbbb"])
    );
    assert_eq!(j.result.as_ref().unwrap()["store_paths_removed"], 1);
    let rels: Vec<ReleaseInfo> = c
        .admin(reqwest::Method::GET, "/admin/v1/releases")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rels.len(), 1);
    if c.svc.has_search {
        let (_, r): (u16, SearchResponse) = c.get("/v1/search?series=unstable").await;
        assert!(r.hits.is_empty());
        let (_, r): (u16, SearchResponse) = c.get("/v1/search?q=hello").await;
        assert_eq!(r.hits[0].releases, vec!["nixos-25.05.100.aaaaaaaaaaaa"]);
    }
    // Prune by commit prefix removes everything else.
    let r = c
        .admin(reqwest::Method::POST, "/admin/v1/prune")
        .json(&json!({"commits": ["aaaa"]}))
        .send()
        .await
        .unwrap();
    let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
    assert_eq!(j.status, JobStatus::Succeeded);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM store_paths")
        .fetch_one(&c.svc.state.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM packages")
        .fetch_one(&c.svc.state.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn admin_token_and_url_ingest() {
    let Some(c) = setup(Some("s3cret")).await else {
        return;
    };
    let r = c
        .http
        .get(format!("{}/admin/v1/releases", c.svc.admin_url))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = c
        .http
        .get(format!("{}/admin/v1/releases", c.svc.admin_url))
        .bearer_auth("wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    // Admin routes are not exposed on the public listener.
    let r = c
        .http
        .get(format!("{}/admin/v1/releases", c.svc.public_url))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // Serve the bundle over HTTP and ingest it by URL.
    let data = encode(&bundle(), Format::Binary(Compression::Xz(1)));
    let app = axum::Router::new().route("/b.nfib", axum::routing::get(move || async move { data.clone() }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/b.nfib", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let r = c
        .admin(reqwest::Method::POST, "/admin/v1/ingest")
        .json(&json!({"url": url}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
    assert_eq!(j.status, JobStatus::Succeeded, "{:?}", j.error);
    let rels: Vec<ReleaseInfo> = c
        .admin(reqwest::Method::GET, "/admin/v1/releases")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rels.len(), 2);

    // A bad upload fails the job with a useful error.
    let r = c
        .admin(reqwest::Method::POST, "/admin/v1/ingest")
        .body("garbage")
        .send()
        .await
        .unwrap();
    let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
    assert_eq!(j.status, JobStatus::Failed);
    assert!(j.error.unwrap().contains("magic"));
    let jobs: Vec<JobInfo> = c
        .admin(reqwest::Method::GET, "/admin/v1/jobs")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(jobs.len(), 2);

    if c.svc.has_search {
        let r = c
            .admin(reqwest::Method::POST, "/admin/v1/reindex")
            .send()
            .await
            .unwrap();
        let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
        assert_eq!(j.status, JobStatus::Succeeded, "{:?}", j.error);
        assert_eq!(j.result.unwrap()["search_documents"], 4);
    }
}

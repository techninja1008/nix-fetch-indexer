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
    let n: i64 = sqlx::query_scalar("SELECT sum(cardinality(closures))::bigint FROM store_paths")
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

#[tokio::test]
async fn consistency_check_and_query_errors() {
    let Some(c) = setup(None).await else { return };
    c.ingest_bytes(encode(&bundle(), Format::Binary(Compression::Zstd(3))))
        .await;
    let pool = &c.svc.state.pool;
    let run_check = || async {
        let r = c
            .admin(reqwest::Method::POST, "/admin/v1/check")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 202);
        let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
        assert_eq!(j.status, JobStatus::Succeeded, "{:?}", j.error);
        serde_json::from_value::<CheckReport>(j.result.unwrap()).unwrap()
    };
    let report = run_check().await;
    assert!(report.ok, "{report:?}");
    assert_eq!(report.store_paths, 6);
    assert_eq!(report.releases, 2);
    let bulk = |attr: &str| json!({"items": [{"attr": attr, "commit": A}]});
    let (s, _): (u16, BulkResponse) = c.post("/v1/lookup/bulk", &bulk("hello")).await;
    assert_eq!(s, 200);

    let exec = |sql: &'static str| async move { sqlx::query(sql).execute(pool).await.unwrap() };
    let hello = format!("{}", h(2));
    let glibc = format!("{}", h(1));
    // 1. A dangling reference: queries touching it fail instead of returning a short closure.
    sqlx::query("UPDATE store_paths SET refs = refs || 999999::bigint WHERE hash = $1")
        .bind(&hello)
        .execute(pool)
        .await
        .unwrap();
    let (s, e): (u16, Value) = c.post("/v1/lookup/bulk", &bulk("hello")).await;
    assert_eq!(s, 500);
    assert!(
        e["error"].as_str().unwrap().contains("index inconsistency"),
        "{e}"
    );
    assert!(
        e["error"]
            .as_str()
            .unwrap()
            .contains("missing store path id 999999"),
        "{e}"
    );
    // Unaffected closures still work.
    let (s, _): (u16, BulkResponse) = c.post("/v1/lookup/bulk", &bulk("curl")).await;
    assert_eq!(s, 200);
    // 2. Membership in an unknown closure, 3. an orphan path, 4. an open closure.
    exec("UPDATE store_paths SET closures = closures || 4242 WHERE name = 'curl-8.0'").await;
    exec("INSERT INTO store_paths (hash, name, closures) VALUES ('orphanorphanorphanorphanorphan00', 'orphan', '{}')").await;
    sqlx::query("UPDATE store_paths SET closures = closures[1:1] WHERE hash = $1")
        .bind(&glibc)
        .execute(pool)
        .await
        .unwrap();

    let report = run_check().await;
    assert!(!report.ok);
    let kinds: Vec<(&str, u64)> = report.issues.iter().map(|i| (i.kind.as_str(), i.count)).collect();
    assert_eq!(
        kinds,
        vec![
            ("dangling_refs", 1),
            ("unknown_closures", 1),
            ("orphan_paths", 1),
            ("open_closures", 4)
        ]
    );
    assert!(report.issues[0].examples[0].contains("missing id 999999"));
    assert!(report.issues[1].examples[0].contains("missing closure id 4242"));

    // A path in no closure is inconsistent too when a query reaches it.
    exec("UPDATE store_paths SET closures = '{}' WHERE name = 'curl-8.0'").await;
    let (s, e): (u16, Value) = c.get(&format!("/v1/packages/curl?commit={A}")).await;
    assert_eq!(s, 500);
    assert!(
        e["error"].as_str().unwrap().contains("belongs to no closure"),
        "{e}"
    );
}

#[tokio::test]
async fn ingest_rejects_closures_of_releases_without_the_dataset() {
    let Some(c) = setup(None).await else { return };
    // Release B carries only packages, but the bundle still has a closure
    // for it: the bundle is invalid and nothing is stored.
    let mut b = bundle();
    b.releases[1].datasets = vec![Dataset::Packages];
    let r = c
        .admin(reqwest::Method::POST, "/admin/v1/ingest")
        .body(encode(&b, Format::Json))
        .send()
        .await
        .unwrap();
    let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
    assert_eq!(j.status, JobStatus::Failed);
    assert!(j.error.unwrap().contains("lacks the closure dataset"));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM store_paths")
        .fetch_one(&c.svc.state.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

/// A helpers-direct closure for release A: a bootstrap tool (path 7) that
/// references glibc (path 1), plus an uncached source tarball.
fn helpers_bundle() -> Bundle {
    let base = bundle();
    let mut b = Bundle::default();
    let mut r = base.releases[0].clone();
    r.datasets = vec![Dataset::Closure];
    let ra = b.ensure_release(r);
    let mut cb = ClosureBuilder::new();
    let ci = cb.ensure_closure(ra, "helpers-direct");
    let bits = Bitset::single(ci);
    for (n, name, refs) in [(7u8, "curl-bootstrap-8.0", vec![1u8]), (1, "glibc-2.40", vec![1])] {
        cb.insert(
            PathInfo {
                hash: h(n),
                name: name.into(),
                references: refs.into_iter().map(h).collect(),
                nar_size: Some(n as u64 * 1000),
                file_size: Some(n as u64 * 100),
                deriver: None,
            },
            &bits,
        );
    }
    cb.add_missing(h(98), "curl-8.0.tar.xz", &bits);
    b.closure = Some(cb.finish());
    b.validate().unwrap();
    b
}

#[tokio::test]
async fn closure_types_ingest_minimise_and_prune() {
    let Some(c) = setup(None).await else { return };
    c.ingest_bytes(encode(&bundle(), Format::Json)).await;
    let job = c.ingest_bytes(encode(&helpers_bundle(), Format::Json)).await;
    let result = job.result.unwrap();
    assert_eq!(result["new_store_paths"], 1);
    assert_eq!(
        result["closures"],
        json!(["nixos-25.05.100.aaaaaaaaaaaa helpers-direct"])
    );

    let releases = || async {
        let rels: Vec<ReleaseInfo> = c
            .admin(reqwest::Method::GET, "/admin/v1/releases")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        rels.into_iter().find(|r| r.commit == A).unwrap()
    };
    let a = releases().await;
    let types: Vec<_> = a.closures.iter().map(|c| c.closure_type.as_str()).collect();
    assert_eq!(types, ["store-paths", "helpers-direct"]);

    // A build-only tool from a helpers closure is a hit.
    let drv = |n: u8, name: &str| format!("{}-{name}.drv", h(100 + n));
    let graph = json!({"version": 4, "derivations": {
        drv(1, "app"): {"outputs": {"out": {"path": format!("{}-app", h(50))}},
            "inputs": {"drvs": {drv(7, "curl-bootstrap-8.0"): {"outputs": ["out"], "dynamicOutputs": {}}}, "srcs": []}},
        drv(7, "curl-bootstrap-8.0"): {"outputs": {"out": {"path": format!("{}-curl-bootstrap-8.0", h(7))}},
            "inputs": {"drvs": {}, "srcs": []}},
    }});
    let (s, m): (u16, MinimiseResponse) = c.post("/v1/minimise", &graph).await;
    assert_eq!(s, 200);
    assert_eq!(m.to_build, vec![format!("/nix/store/{}", drv(1, "app"))]);
    assert_eq!(m.roots, vec![path(7, "curl-bootstrap-8.0")]);

    // Pruning one closure type keeps the release and its other closures.
    let r = c
        .admin(reqwest::Method::POST, "/admin/v1/prune")
        .json(&json!({"channels": ["nixos-25.05"], "closure_types": ["helpers-direct"]}))
        .send()
        .await
        .unwrap();
    let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
    assert_eq!(j.status, JobStatus::Succeeded, "{:?}", j.error);
    let res = j.result.unwrap();
    assert_eq!(res["releases"], json!([]));
    assert_eq!(
        res["closures"],
        json!(["nixos-25.05.100.aaaaaaaaaaaa helpers-direct"])
    );
    assert_eq!(res["store_paths_removed"], 1);
    let a = releases().await;
    assert_eq!(a.closures.len(), 1);
    assert_eq!(a.closures[0].closure_type, "store-paths");
    assert!(a.datasets.contains(&"closure".to_string()));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM missing_paths")
        .fetch_one(&c.svc.state.pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "only the store-paths closure's missing path remains");
    let (_, m): (u16, MinimiseResponse) = c.post("/v1/minimise", &graph).await;
    assert_eq!(m.to_build.len(), 2);

    // A differently built curl-8.0 (path 3 is indexed as curl-8.0) is a close
    // miss; an uncached fixed-output source is a fetch, not a build.
    let graph = json!({"version": 4, "derivations": {
        drv(1, "app"): {"outputs": {"out": {"path": format!("{}-app", h(50))}},
            "inputs": {"drvs": {drv(3, "curl-8.0"): {"outputs": ["out"], "dynamicOutputs": {}}}, "srcs": []}},
        drv(3, "curl-8.0"): {"outputs": {"out": {"path": format!("{}-curl-8.0", h(53))}},
            "inputs": {"drvs": {drv(8, "curl-8.0.tar.xz"): {"outputs": ["out"], "dynamicOutputs": {}}}, "srcs": []}},
        drv(8, "curl-8.0.tar.xz"): {"outputs": {"out": {"path": format!("{}-curl-8.0.tar.xz", h(54)),
            "hashAlgo": "sha256", "hash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}},
            "inputs": {"drvs": {}, "srcs": []}},
    }});
    let (s, m): (u16, MinimiseResponse) = c.post("/v1/minimise", &graph).await;
    assert_eq!(s, 200);
    assert_eq!(m.to_build.len(), 2);
    assert_eq!(
        m.to_fetch,
        vec![format!("/nix/store/{}", drv(8, "curl-8.0.tar.xz"))]
    );
    let w: Vec<_> = m.warnings.iter().filter(|w| w.code == "close_miss").collect();
    assert_eq!(w.len(), 1, "{:?}", m.warnings);
    assert_eq!(w[0].paths, vec![format!("/nix/store/{}", drv(3, "curl-8.0"))]);
    assert!(w[0].message.contains("curl-8.0 (1 indexed)"), "{}", w[0].message);
    let (_, m): (u16, MinimiseResponse) = c.post("/v1/minimise?close_miss_threshold=0", &graph).await;
    assert!(m.warnings.iter().all(|w| w.code != "close_miss"));

    // A version 2 bundle is read with the closure type given at ingest.
    let mut v2 = helpers_bundle();
    v2.format_version = 2;
    v2.closure.as_mut().unwrap().closures.clear();
    let r = c
        .admin(
            reqwest::Method::POST,
            "/admin/v1/ingest?v2_closure_type=helpers-full",
        )
        .body(encode(&v2, Format::Json))
        .send()
        .await
        .unwrap();
    let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
    assert_eq!(j.status, JobStatus::Succeeded, "{:?}", j.error);
    let a = releases().await;
    let types: Vec<_> = a.closures.iter().map(|c| c.closure_type.as_str()).collect();
    assert_eq!(types, ["store-paths", "helpers-full"]);

    let r = c
        .admin(reqwest::Method::POST, "/admin/v1/check")
        .send()
        .await
        .unwrap();
    let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
    let report: CheckReport = serde_json::from_value(j.result.unwrap()).unwrap();
    assert!(report.ok, "{report:?}");
    assert_eq!(report.closures, 3);
}

/// Helper probe eval errors for both releases: fetchhg fails in A and B,
/// fetchsvn only in A.
fn eval_errors_bundle() -> Bundle {
    let base = bundle();
    let mut b = Bundle::default();
    let mut e = EvalErrorsSection::default();
    for (i, r) in base.releases.iter().enumerate() {
        let mut r = r.clone();
        r.datasets = vec![Dataset::EvalErrors];
        let ri = b.ensure_release(r);
        let ev = e.ensure_eval(ri as u32, "helper-probes");
        e.add_error("fetchhg", "x86_64-linux", ev);
        if i == 0 {
            e.add_error("fetchsvn", "aarch64-linux", ev);
        }
    }
    b.eval_errors = Some(e);
    b.validate().unwrap();
    b
}

#[tokio::test]
async fn eval_errors_ingest_and_prune() {
    let Some(c) = setup(None).await else { return };
    c.ingest_bytes(encode(&bundle(), Format::Json)).await;
    let job = c.ingest_bytes(encode(&eval_errors_bundle(), Format::Json)).await;
    let result = job.result.unwrap();
    assert_eq!(result["eval_errors"], 3);
    assert_eq!(result["evals"].as_array().unwrap().len(), 2);
    // Re-ingesting is a no-op.
    c.ingest_bytes(encode(&eval_errors_bundle(), Format::Json)).await;

    let releases = || async {
        let mut rels: Vec<ReleaseInfo> = c
            .admin(reqwest::Method::GET, "/admin/v1/releases")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        rels.sort_by(|a, b| a.commit.cmp(&b.commit));
        rels
    };
    let rels = releases().await;
    assert_eq!(rels[0].commit, A);
    assert!(rels[0].datasets.contains(&"eval-errors".to_string()));
    assert_eq!(rels[0].evals.len(), 1);
    assert_eq!(rels[0].evals[0].eval_type, "helper-probes");
    assert_eq!(
        rels[0].evals[0].errors,
        ["fetchhg@x86_64-linux", "fetchsvn@aarch64-linux"]
    );
    assert_eq!(rels[1].evals[0].errors, ["fetchhg@x86_64-linux"]);

    let check = || async {
        let r = c
            .admin(reqwest::Method::POST, "/admin/v1/check")
            .send()
            .await
            .unwrap();
        let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
        serde_json::from_value::<CheckReport>(j.result.unwrap()).unwrap()
    };
    let report = check().await;
    assert!(report.ok, "{report:?}");
    assert_eq!((report.evals, report.eval_errors), (2, 3));

    // Pruning a release removes its evaluations and their errors.
    let r = c
        .admin(reqwest::Method::POST, "/admin/v1/prune")
        .json(&json!({"commits": [A]}))
        .send()
        .await
        .unwrap();
    let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
    assert_eq!(j.status, JobStatus::Succeeded, "{:?}", j.error);
    let report = check().await;
    assert_eq!((report.evals, report.eval_errors), (1, 1));
}

/// Migration 0003 turns existing release membership into store-paths
/// closures with the same ids, without rewriting path rows.
#[tokio::test]
async fn closure_types_migration_keeps_ids() {
    let Some(url) = nfi_testenv::fresh_database().await else {
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
    let run = |f: &str| {
        let pool = pool.clone();
        let sql = std::fs::read_to_string(format!("{dir}/{f}")).unwrap();
        async move { sqlx::raw_sql(&sql).execute(&pool).await.unwrap() }
    };
    run("0001_init.sql").await;
    run("0002_compact_closure.sql").await;
    sqlx::raw_sql(
        "INSERT INTO releases (id, name, commit, channel, series) VALUES
           (3, 'r3', 'c3', 'nixos-25.05', '25.05'), (5, 'r5', 'c5', 'nixos-25.05', '25.05'),
           (7, 'r7', 'c7', 'nixos-25.05', '25.05');
         SELECT setval(pg_get_serial_sequence('releases', 'id'), 7);
         INSERT INTO release_datasets VALUES (3, 'closure'), (5, 'closure'), (7, 'packages');
         INSERT INTO store_paths (hash, name, releases) VALUES ('h1', 'a', '{3,5}'), ('h2', 'b', '{5}');
         INSERT INTO missing_paths (hash, name, release_id) VALUES ('h9', 'gone', 3);",
    )
    .execute(&pool)
    .await
    .unwrap();
    run("0003_closure_types.sql").await;
    let closures: Vec<(i32, i32, String)> = sqlx::query_as(
        "SELECT c.id, c.release_id, t.name FROM closures c JOIN closure_types t ON t.id = c.type_id ORDER BY c.id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        closures,
        vec![(3, 3, "store-paths".into()), (5, 5, "store-paths".into())]
    );
    let paths: Vec<(String, Vec<i32>)> =
        sqlx::query_as("SELECT hash, closures FROM store_paths ORDER BY hash")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(paths, vec![("h1".into(), vec![3, 5]), ("h2".into(), vec![5])]);
    let missing: Vec<(String, i32)> = sqlx::query_as("SELECT hash, closure_id FROM missing_paths")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(missing, vec![("h9".into(), 3)]);
    // New closures get ids past every release id, so they can't collide
    // with ids still being assigned 1:1.
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO closures (release_id, type_id) SELECT 7, id FROM closure_types RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(id > 7, "{id}");
}

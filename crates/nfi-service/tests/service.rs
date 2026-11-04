//! Integration tests against ephemeral Postgres and Meilisearch.

use nfi_api::*;
use nfi_interchange::*;
use serde_json::{Value, json};
use std::time::Duration;

use nfi_testenv::{A, B, bundle, h, path};

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
    // The public listener serves the same list, optionally by channel.
    let (st, public): (u16, Vec<ReleaseInfo>) = c.get("/v1/releases").await;
    assert_eq!(st, 200);
    assert_eq!(
        serde_json::to_value(&public).unwrap(),
        serde_json::to_value(&rels).unwrap()
    );
    let (_, unstable): (u16, Vec<ReleaseInfo>) = c.get("/v1/releases?channel=nixos-unstable").await;
    assert_eq!(unstable.len(), 1);
    assert_eq!(unstable[0].name, "nixos-26.11pre200.bbbbbbbbbbbb");
    let (_, none): (u16, Vec<ReleaseInfo>) = c.get("/v1/releases?channel=nope").await;
    assert!(none.is_empty());

    // Direct lookup.
    let (s, p): (u16, PackageInfo) = c.get(&format!("/v1/packages/curl?commit={A}")).await;
    assert_eq!(s, 200);
    assert_eq!(p.version.as_deref(), Some("8.0"));
    assert_eq!(p.release.channel, "nixos-25.05");
    assert_eq!(p.release.commit, A);
    assert!(p.outputs["out"].cached);
    assert!(!p.outputs["dev"].cached);
    assert_eq!(p.programs, vec!["curl"]);
    // A full hash in upper case resolves too.
    let (s, p): (u16, PackageInfo) = c
        .get(&format!("/v1/packages/curl?commit={}", A.to_uppercase()))
        .await;
    assert_eq!(s, 200);
    assert_eq!(p.release.commit, A);
    // Not at release B.
    let (s, _): (u16, Value) = c.get(&format!("/v1/packages/curl?commit={B}")).await;
    assert_eq!(s, 404);
    // So does a unique prefix of at least 8 hex digits, in any case.
    for prefix in ["aaaaaaaa", "AAAAaaaaAAAA"] {
        let (s, p): (u16, PackageInfo) = c.get(&format!("/v1/packages/curl?commit={prefix}")).await;
        assert_eq!(s, 200, "{prefix}");
        assert_eq!(p.release.commit, A);
    }
    // A prefix (or full hash) of no release is an unknown commit.
    for unknown in ["cccccccc".to_string(), "c".repeat(40), format!("{A}a")] {
        let (s, e): (u16, ErrorBody) = c.get(&format!("/v1/packages/curl?commit={unknown}")).await;
        assert_eq!(s, 404);
        assert_eq!(e.error, format!("unknown commit {unknown}"));
    }
    // Fewer than 8 digits, or a non-hex digit, is a request error.
    for bad in ["aaaaaaa", "", "aaaaaaag"] {
        let (s, e): (u16, ErrorBody) = c.get(&format!("/v1/packages/curl?commit={bad}")).await;
        assert_eq!(s, 400, "{bad:?}");
        assert_eq!(
            e.error,
            format!("invalid commit {bad:?}: expected a hex commit prefix of at least 8 characters")
        );
    }

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
                {"attr": "hello", "commit": "d".repeat(40)},
                {"attr": "hello", "commit": A.to_uppercase()},
                {"attr": "curl", "commit": A, "outputs": ["lib"]},
                {"attr": "curl", "commit": A, "outputs": ["out", "lib", "man"]},
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
            ItemStatus::UnknownCommit,
            ItemStatus::Ok,
            ItemStatus::UnknownOutput,
            ItemStatus::UnknownOutput,
        ]
    );
    // A requested output that doesn't exist fails the item, which names the
    // missing outputs and the existing ones, and selects nothing.
    for (i, missing) in [(7, vec!["lib"]), (8, vec!["lib", "man"])] {
        let it = &bulk.items[i];
        assert_eq!(it.missing_outputs, missing, "item {i}");
        assert_eq!(it.available_outputs, ["dev", "out"], "item {i}");
        assert!(it.outputs.is_empty(), "item {i}");
    }
    // Items echo the release's (lowercase) commit once resolved.
    assert_eq!(bulk.items[6].commit, A);
    assert_eq!(bulk.items[6].release, bulk.items[0].release);
    // Any commit that is not a full hash fails the whole request, naming each
    // offending item.
    let (s, e): (u16, ErrorBody) = c
        .post(
            "/v1/lookup/bulk",
            &json!({"items": [
                {"attr": "hello", "commit": A},
                {"attr": "hello", "commit": "aaaaaaaa"},
                {"attr": "curl", "commit": "d".repeat(40)},
                {"attr": "curl", "commit": "zz"},
            ]}),
        )
        .await;
    assert_eq!(s, 400);
    assert_eq!(
        e.error,
        "invalid commit in item 1 (hello@aaaaaaaa), item 3 (curl@zz): \
         expected full 40-character hex commit hashes"
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
    let reduced = nfi_drv::reduce(&graph).unwrap();
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
    let r = c
        .http
        .post(format!("{}/v1/minimise", c.svc.public_url))
        .header("content-type", "text/plain")
        .body(serde_json::to_vec(&reduced).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 415);

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
        // The commit selects a release like package lookups do: a prefix, in
        // either case, of the full hash the documents list.
        for commit in [A[..8].to_string(), A.to_uppercase(), A[..12].to_uppercase()] {
            let (s, r): (u16, SearchResponse) =
                c.get(&format!("/v1/search?q=transfer&commit={commit}")).await;
            assert_eq!((s, r.hits.len()), (200, 1), "{commit}");
        }
        let (s, r): (u16, SearchResponse) = c.get(&format!("/v1/search?q=transfer&commit={}", &B[..8])).await;
        assert_eq!((s, r.hits.len()), (200, 0));
        for bad in ["aaaa", "zzzzzzzz", "aaaaaaaa-"] {
            let (s, e): (u16, ErrorBody) = c.get(&format!("/v1/search?commit={bad}")).await;
            assert_eq!(s, 400, "{bad}");
            assert!(e.error.contains("expected a hex commit prefix"), "{}", e.error);
        }
        let (s, e): (u16, ErrorBody) = c.get(&format!("/v1/search?commit={}", "d".repeat(12))).await;
        assert_eq!(s, 404);
        assert_eq!(e.error, format!("unknown commit {}", "d".repeat(12)));
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
        .json(&json!({"commits": ["aaaaaaaa"]}))
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

/// A package lookup's commit prefix must select exactly one release.
#[tokio::test]
async fn package_lookup_rejects_ambiguous_commit_prefix() {
    let Some(c) = setup(None).await else { return };
    sqlx::raw_sql(&format!(
        "INSERT INTO releases (name, commit, channel, series) VALUES
           ('r1', 'abcdef01{a}', 'nixos-25.05', '25.05'),
           ('r2', 'abcdef01{b}', 'nixos-25.05', '25.05'),
           ('r3', 'abcdef02{a}', 'nixos-25.05', '25.05')",
        a = &A[8..],
        b = &B[8..]
    ))
    .execute(&c.svc.state.pool)
    .await
    .unwrap();
    let (s, e): (u16, ErrorBody) = c.get("/v1/packages/curl?commit=ABCDEF01").await;
    assert_eq!(s, 400);
    assert_eq!(
        e.error,
        "commit prefix \"ABCDEF01\" is ambiguous: it matches 2 releases"
    );
    // A longer prefix disambiguates; the release has no curl.
    let (s, e): (u16, ErrorBody) = c
        .get(&format!("/v1/packages/curl?commit=abcdef01{}", &A[8..12]))
        .await;
    assert_eq!(s, 404);
    assert!(e.error.contains("not found at"), "{}", e.error);
    let (s, e): (u16, ErrorBody) = c.get("/v1/packages/curl?commit=abcdef02").await;
    assert_eq!(s, 404);
    assert_eq!(e.error, "curl (x86_64-linux) not found at abcdef02");
    // Search selects releases the same way.
    if c.svc.has_search {
        let (s, e): (u16, ErrorBody) = c.get("/v1/search?q=x&commit=ABCDEF01").await;
        assert_eq!(s, 400);
        assert_eq!(
            e.error,
            "commit prefix \"ABCDEF01\" is ambiguous: it matches 2 releases"
        );
        let (s, _): (u16, ErrorBody) = c.get("/v1/search?commit=abcdef03").await;
        assert_eq!(s, 404);
    }
}

/// Bulk requests are capped at `BULK_MAX_ITEMS` items, and items share
/// queries: many items over a few commits resolve like one each.
#[tokio::test]
async fn bulk_item_cap_and_batching() {
    let Some(c) = setup(None).await else { return };
    c.ingest_bytes(encode(&bundle(), Format::Json)).await;
    let item = |attr: &str, commit: &str| json!({"attr": attr, "commit": commit});
    let over: Vec<Value> = (0..=BULK_MAX_ITEMS).map(|_| item("hello", A)).collect();
    let (s, e): (u16, ErrorBody) = c.post("/v1/lookup/bulk", &json!({ "items": over })).await;
    assert_eq!(s, 400);
    assert_eq!(
        e.error,
        format!(
            "too many items: {} (at most {BULK_MAX_ITEMS} per request; split the request)",
            BULK_MAX_ITEMS + 1
        )
    );
    // At the cap: every item is answered, in order.
    let attrs = ["hello", "curl", "missing", "nooutpaths"];
    let commits = [A.to_string(), B.to_string(), "d".repeat(40)];
    let items: Vec<Value> = (0..BULK_MAX_ITEMS)
        .map(|i| item(attrs[i % 4], &commits[i % 3]))
        .collect();
    let (s, bulk): (u16, BulkResponse) = c.post("/v1/lookup/bulk", &json!({ "items": items })).await;
    assert_eq!(s, 200);
    assert_eq!(bulk.items.len(), BULK_MAX_ITEMS);
    for (i, it) in bulk.items.iter().enumerate() {
        let expect = match (attrs[i % 4], i % 3) {
            (_, 2) => ItemStatus::UnknownCommit,
            ("hello", _) | ("curl", 0) => ItemStatus::Ok,
            ("nooutpaths", 0) => ItemStatus::NoOutpaths,
            _ => ItemStatus::UnknownAttr,
        };
        assert_eq!((it.attr.as_str(), it.status), (attrs[i % 4], expect), "item {i}");
    }
    let mut expect = vec![
        path(1, "glibc-2.40"),
        path(2, "hello-2.12"),
        path(3, "curl-8.0"),
        path(4, "openssl-3.0"),
    ];
    expect.sort();
    assert_eq!(bulk.closure, expect);
}

/// Commit prefixes must have at least 8 hex digits and match at most one
/// release; one matching none prunes nothing.
#[tokio::test]
async fn prune_by_unambiguous_commit_prefix() {
    let Some(c) = setup(None).await else { return };
    let pool = &c.svc.state.pool;
    sqlx::raw_sql(&format!(
        "INSERT INTO releases (name, commit, channel, series) VALUES
           ('r1', 'abcdef01{a}', 'nixos-25.05', '25.05'),
           ('r2', 'abcdef01{b}', 'nixos-25.05', '25.05'),
           ('r3', 'abcdef02{a}', 'nixos-25.05', '25.05')",
        a = &A[8..],
        b = &B[8..]
    ))
    .execute(pool)
    .await
    .unwrap();
    let names = || async {
        sqlx::query_scalar::<_, String>("SELECT name FROM releases ORDER BY name")
            .fetch_all(pool)
            .await
            .unwrap()
    };
    let prune = |commits: &[&str]| {
        c.admin(reqwest::Method::POST, "/admin/v1/prune")
            .json(&json!({ "commits": commits }))
            .send()
    };

    let r = prune(&["abcdef0"]).await.unwrap();
    assert_eq!(r.status(), 400);
    let e: ErrorBody = r.json().await.unwrap();
    assert!(e.error.contains("at least 8"), "{}", e.error);

    let r = prune(&["abcdef02", "ABCDEF01"]).await.unwrap();
    assert_eq!(r.status(), 400);
    let e: ErrorBody = r.json().await.unwrap();
    assert!(
        e.error.contains("\"ABCDEF01\"") && e.error.contains("matches 2 releases"),
        "{}",
        e.error
    );
    assert_eq!(names().await, ["r1", "r2", "r3"]);
    // The job checks again (a release may have been ingested after the
    // request was queued).
    let req = PruneRequest {
        commits: vec!["abcdef01".into()],
        ..Default::default()
    };
    let e = nfi_service::prune::prune(pool, &req).await.unwrap_err();
    assert!(e.to_string().contains("matches 2 releases"), "{e}");
    assert_eq!(names().await, ["r1", "r2", "r3"]);

    for (commits, pruned) in [(["ABCDEF02"], vec!["r3"]), (["12345678"], vec![])] {
        let r = prune(&commits).await.unwrap();
        assert_eq!(r.status(), 202);
        let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
        assert_eq!(j.status, JobStatus::Succeeded, "{:?}", j.error);
        assert_eq!(j.result.unwrap()["releases"], json!(pruned));
    }
    assert_eq!(names().await, ["r1", "r2"]);
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

/// The qualified closure types of a release's evaluations.
fn closure_types(r: &ReleaseInfo) -> Vec<String> {
    r.evals
        .iter()
        .flat_map(|e| {
            e.closures
                .iter()
                .map(|c| ClosureType::new(&e.eval_type, c).to_string())
        })
        .collect()
}

/// A helpers:direct closure for release A: a bootstrap tool (path 7) that
/// references glibc (path 1), plus an uncached source tarball.
fn helpers_bundle() -> Bundle {
    let base = bundle();
    let mut b = Bundle::default();
    let mut r = base.releases[0].clone();
    r.datasets = vec![Dataset::Closure];
    let ra = b.ensure_release(r);
    let e = b.ensure_eval(ra as u32, "helpers");
    let mut cb = ClosureBuilder::new();
    let ci = cb.ensure_closure(e, "direct");
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
        json!(["nixos-25.05.100.aaaaaaaaaaaa helpers:direct"])
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
    // Evaluations are listed in eval type creation order.
    assert_eq!(closure_types(&a), ["store-paths", "helpers:direct"]);
    // The helpers evaluation's errors were not part of the bundle.
    assert_eq!(a.evals[1].errors, None);

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
        .json(&json!({"channels": ["nixos-25.05"], "closure_types": ["helpers:direct"]}))
        .send()
        .await
        .unwrap();
    let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
    assert_eq!(j.status, JobStatus::Succeeded, "{:?}", j.error);
    let res = j.result.unwrap();
    assert_eq!(res["releases"], json!([]));
    assert_eq!(
        res["closures"],
        json!(["nixos-25.05.100.aaaaaaaaaaaa helpers:direct"])
    );
    assert_eq!(res["store_paths_removed"], 1);
    let a = releases().await;
    // The helpers evaluation had nothing else, so it went too.
    assert_eq!(closure_types(&a), ["store-paths"]);
    assert_eq!(a.evals.len(), 1);
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
    v2.evals.clear();
    let r = c
        .admin(
            reqwest::Method::POST,
            "/admin/v1/ingest?v2_closure_type=helpers:full",
        )
        .body(encode(&v2, Format::Json))
        .send()
        .await
        .unwrap();
    let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
    assert_eq!(j.status, JobStatus::Succeeded, "{:?}", j.error);
    let a = releases().await;
    assert_eq!(closure_types(&a), ["store-paths", "helpers:full"]);

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
    let mut e = EvalErrorsBuilder::default();
    for (i, r) in base.releases.iter().enumerate() {
        let mut r = r.clone();
        r.datasets = vec![Dataset::EvalErrors];
        let ri = b.ensure_release(r);
        let ev = b.ensure_eval(ri as u32, "helpers");
        e.add("fetchhg", "x86_64-linux", ev);
        if i == 0 {
            e.add("fetchsvn", "aarch64-linux", ev);
        }
    }
    b.eval_errors = Some(e.finish());
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
            .admin(reqwest::Method::GET, "/admin/v1/releases?errors=true")
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
    // The store-paths pseudo-evaluation (from the first bundle) has no
    // error information; the helpers evaluation has no closures.
    let ev: Vec<_> = rels[0]
        .evals
        .iter()
        .map(|e| {
            (
                e.eval_type.as_str(),
                e.closures.len(),
                e.error_count,
                e.errors.clone(),
            )
        })
        .collect();
    assert_eq!(
        ev,
        [
            ("store-paths", 1, None, None),
            (
                "helpers",
                0,
                Some(2),
                Some(vec![
                    "fetchhg@x86_64-linux".to_string(),
                    "fetchsvn@aarch64-linux".to_string()
                ])
            ),
        ]
    );
    assert_eq!(
        rels[1].evals[1].errors,
        Some(vec!["fetchhg@x86_64-linux".to_string()])
    );
    // Without errors=true only the counts are listed, here and publicly.
    let (_, public): (u16, Value) = c.get("/v1/releases?channel=nixos-25.05").await;
    assert_eq!(public[0]["evals"][1]["error_count"], 2);
    assert!(public[0]["evals"][1].get("errors").is_none(), "{public}");
    assert_eq!(public[0]["evals"][0]["error_count"], Value::Null);
    let (_, public): (u16, Vec<ReleaseInfo>) = c.get("/v1/releases?channel=nixos-25.05&errors=true").await;
    assert_eq!(public[0].evals[1].errors, rels[0].evals[1].errors);

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
    assert_eq!((report.evals, report.eval_errors), (4, 3));

    // Pruning the helpers closures of a release keeps its helpers
    // evaluation while its errors are known.
    let r = c
        .admin(reqwest::Method::POST, "/admin/v1/prune")
        .json(&json!({"commits": [A], "closure_types": ["helpers:direct", "helpers:full"]}))
        .send()
        .await
        .unwrap();
    let j = c.wait_job(r.json::<JobCreated>().await.unwrap().id).await;
    assert_eq!(j.status, JobStatus::Succeeded, "{:?}", j.error);
    assert_eq!(check().await.evals, 4);

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
    assert!(report.ok, "{report:?}");
    assert_eq!((report.evals, report.eval_errors), (2, 1));
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
    // Each release with the closure dataset gets a store-paths evaluation
    // and its unnamed closure, both with the release's id.
    let closures: Vec<(i32, i32, i32, String, String)> = sqlx::query_as(
        "SELECT c.id, e.id, e.release_id, et.name, ct.name FROM closures c
           JOIN evals e ON e.id = c.eval_id
           JOIN eval_types et ON et.id = e.type_id
           JOIN closure_types ct ON ct.id = c.type_id AND ct.eval_type_id = et.id
          ORDER BY c.id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        closures,
        vec![
            (3, 3, 3, "store-paths".into(), "".into()),
            (5, 5, 5, "store-paths".into(), "".into())
        ]
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
    // New evaluations and closures get ids past every release id, so they
    // can't collide with ids still being assigned 1:1.
    let eval: i32 = sqlx::query_scalar(
        "INSERT INTO evals (release_id, type_id) SELECT 7, id FROM eval_types RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(eval > 7, "{eval}");
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO closures (eval_id, type_id) SELECT $1, id FROM closure_types RETURNING id",
    )
    .bind(eval)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(id > 7, "{id}");
}

/// A fresh, migrated database without a service (so no worker claims jobs).
async fn job_pool() -> Option<sqlx::PgPool> {
    let url = nfi_testenv::fresh_database().await?;
    Some(nfi_service::connect(&url, 5).await.unwrap())
}

/// Queue an ingest job with a staged upload and claim it.
async fn claim_staged_job(pool: &sqlx::PgPool) -> nfi_service::jobs::Claim {
    let id: i64 = sqlx::query_scalar("INSERT INTO jobs (kind) VALUES ('ingest') RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO job_blob_chunks (job_id, seq, data) VALUES ($1, 0, 'bundle')")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    let job = nfi_service::jobs::claim(pool, "host-1", Duration::from_secs(120))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.id, id);
    job
}

/// Another worker reclaims the job (with the same worker name, as in
/// containers where every replica is pid 1).
async fn reclaim(pool: &sqlx::PgPool, id: i64) {
    sqlx::query("UPDATE jobs SET claim_id = gen_random_uuid() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

/// (status, progress, result, blob chunks) of a job.
async fn job_state(pool: &sqlx::PgPool, id: i64) -> (String, Option<String>, Option<Value>, i64) {
    sqlx::query_as(
        "SELECT status, progress, result, (SELECT count(*) FROM job_blob_chunks WHERE job_id = id)
           FROM jobs WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A worker whose job was reclaimed can't overwrite the job's result or
/// progress, nor delete its staged upload; the new claim can.
#[tokio::test]
async fn reclaimed_job_is_fenced() {
    use nfi_service::jobs;
    let Some(pool) = job_pool().await else { return };
    let old = claim_staged_job(&pool).await;
    reclaim(&pool, old.id).await;
    assert!(!jobs::touch(&pool, &old, Some("stale")).await.unwrap());
    assert!(
        !jobs::finish(&pool, &old, &Ok(json!({"from": "old"})), None)
            .await
            .unwrap()
    );
    let err: anyhow::Result<Value> = Err(anyhow::anyhow!("old worker failed"));
    assert!(!jobs::finish(&pool, &old, &err, Some("stale")).await.unwrap());
    assert_eq!(job_state(&pool, old.id).await, ("running".into(), None, None, 1));

    // The current claim records its result and drops the upload.
    sqlx::query("UPDATE jobs SET heartbeat_at = now() - interval '1 hour' WHERE id = $1")
        .bind(old.id)
        .execute(&pool)
        .await
        .unwrap();
    let new = jobs::claim(&pool, "host-1", Duration::from_secs(120))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new.id, old.id);
    assert!(jobs::touch(&pool, &new, Some("working")).await.unwrap());
    assert!(
        jobs::finish(&pool, &new, &Ok(json!({"from": "new"})), None)
            .await
            .unwrap()
    );
    assert_eq!(
        job_state(&pool, new.id).await,
        ("succeeded".into(), None, Some(json!({"from": "new"})), 0)
    );
    // The old claim stays fenced off after the job finished.
    assert!(
        !jobs::finish(&pool, &old, &Ok(json!({"from": "old"})), None)
            .await
            .unwrap()
    );
    assert_eq!(job_state(&pool, old.id).await.2, Some(json!({"from": "new"})));
}

/// When the keep-alive notices the job was reclaimed, the running job is
/// cancelled and nothing is recorded.
#[tokio::test]
async fn reclaimed_job_is_cancelled() {
    use nfi_service::jobs;
    let Some(pool) = job_pool().await else { return };
    let job = claim_staged_job(&pool).await;
    let id = job.id;
    let (reached_tx, reached) = tokio::sync::oneshot::channel();
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct OnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let guard = OnDrop(dropped.clone());
    let p2 = pool.clone();
    let run = tokio::spawn(async move {
        jobs::execute(&p2, job, |_, progress| async move {
            let _guard = guard;
            let mut reached_tx = Some(reached_tx);
            // Keep reporting progress so a write after the reclaim notices it
            // (the first step may already be written before the reclaim).
            let mut i = 0u64;
            loop {
                i += 1;
                progress.set(format!("step {i}"));
                if let Some(tx) = reached_tx.take() {
                    tx.send(()).unwrap();
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
    });
    reached.await.unwrap();
    reclaim(&pool, id).await;
    // The next progress write (within about a second) finds the claim gone.
    let recorded = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("job was not cancelled")
        .unwrap();
    assert!(!recorded);
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    // Progress may hold a step written before the reclaim; what matters is
    // that the old claim recorded nothing and left the blob alone.
    let (status, _, result, blobs) = job_state(&pool, id).await;
    assert_eq!((status.as_str(), result, blobs), ("running", None, 1));
}

/// Progress is published while a job runs and cleared when it succeeds; no
/// late progress write overwrites that.
#[tokio::test]
async fn progress_cleared_after_job_finishes() {
    use nfi_service::jobs;
    let Some(pool) = job_pool().await else { return };
    let job = claim_staged_job(&pool).await;
    let id = job.id;
    let p2 = pool.clone();
    let recorded = jobs::execute(&pool, job, |_, progress| async move {
        progress.set("step 1".into());
        let mut seen = None;
        for _ in 0..50 {
            seen = job_state(&p2, id).await.1;
            if seen.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(seen.as_deref(), Some("step 1"));
        // Pending when the job ends: must not be written after the result.
        progress.set("step 2".into());
        Ok(json!({"ok": true}))
    })
    .await;
    assert!(recorded);
    let after = job_state(&pool, id).await;
    assert_eq!(after, ("succeeded".into(), None, Some(json!({"ok": true})), 0));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(job_state(&pool, id).await, after);

    // A failed job keeps its latest progress, including one still pending.
    let job = claim_staged_job(&pool).await;
    let id = job.id;
    let recorded = jobs::execute(&pool, job, |_, progress| async move {
        progress.set("decoding".into());
        anyhow::bail!("bad bundle")
    })
    .await;
    assert!(recorded);
    let (status, progress, _, blobs) = job_state(&pool, id).await;
    assert_eq!(
        (status.as_str(), progress.as_deref(), blobs),
        ("failed", Some("decoding"), 0)
    );
}

/// Each claim counts an attempt; a job whose `MAX_ATTEMPTS`th claim goes
/// stale (its worker died running it) is failed, not claimed again, and its
/// upload freed.
#[tokio::test]
async fn poison_job_fails_after_max_attempts() {
    use nfi_service::jobs;
    let Some(pool) = job_pool().await else { return };
    let stale = Duration::from_secs(120);
    let job = claim_staged_job(&pool).await;
    let id = job.id;
    let attempts = || async {
        sqlx::query_scalar::<_, i32>("SELECT attempts FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert_eq!(attempts().await, 1);
    let go_stale = || async {
        sqlx::query("UPDATE jobs SET heartbeat_at = now() - interval '1 hour' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    };
    for n in 2..=jobs::MAX_ATTEMPTS {
        go_stale().await;
        assert!(jobs::reap(&pool, stale).await.unwrap().is_empty());
        let again = jobs::claim(&pool, "host-2", stale).await.unwrap().unwrap();
        assert_eq!(again.id, id);
        assert_eq!(attempts().await, n);
    }
    // A live claim is left alone.
    assert!(jobs::reap(&pool, stale).await.unwrap().is_empty());
    go_stale().await;
    assert!(jobs::claim(&pool, "host-3", stale).await.unwrap().is_none());
    assert_eq!(jobs::reap(&pool, stale).await.unwrap(), vec![id]);
    let (status, _, _, blobs) = job_state(&pool, id).await;
    assert_eq!((status.as_str(), blobs), ("failed", 0));
    let info = jobs::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(info.status, JobStatus::Failed);
    let error = info.error.unwrap();
    assert!(
        error.starts_with(&format!("abandoned after {} attempts", jobs::MAX_ATTEMPTS)),
        "{error}"
    );
    assert!(info.finished_at.is_some());
    // The last claim is fenced off.
    assert!(!jobs::touch(&pool, &job, None).await.unwrap());
}

/// An upload that stops receiving data is failed after `STAGING_TIMEOUT`
/// and its chunks freed; its uploader can't store more. A live upload is
/// left alone.
#[tokio::test]
async fn abandoned_upload_expires() {
    use nfi_service::jobs;
    let Some(pool) = job_pool().await else { return };
    let stale = Duration::from_secs(120);
    // An upload whose sender stalls after the first chunk.
    let (tx, rx) = futures::channel::mpsc::unbounded::<Result<bytes::Bytes, std::io::Error>>();
    tx.unbounded_send(Ok(vec![1u8; jobs::CHUNK].into())).unwrap();
    let p2 = pool.clone();
    let upload = tokio::spawn(async move { jobs::enqueue_upload(&p2, rx, json!({})).await });
    let id: i64 = loop {
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT id, (SELECT count(*) FROM job_blob_chunks WHERE job_id = id) FROM jobs
              WHERE status = 'staging'",
        )
        .fetch_optional(&pool)
        .await
        .unwrap();
        if let Some((id, 1)) = row {
            break id;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let info = jobs::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(info.status, JobStatus::Queued);
    // Still receiving: kept.
    assert!(jobs::reap(&pool, stale).await.unwrap().is_empty());
    sqlx::query("UPDATE jobs SET heartbeat_at = now() - make_interval(secs => $2) WHERE id = $1")
        .bind(id)
        .bind(jobs::STAGING_TIMEOUT.as_secs_f64() + 1.0)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(jobs::reap(&pool, stale).await.unwrap(), vec![id]);
    let info = jobs::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(info.status, JobStatus::Failed);
    assert!(
        info.error.as_deref().unwrap().starts_with("upload abandoned"),
        "{:?}",
        info.error
    );
    assert_eq!(job_state(&pool, id).await.3, 0);
    // The uploader resumes: its next chunk is refused, nothing is stored and
    // the job stays failed.
    tx.unbounded_send(Ok(vec![2u8; jobs::CHUNK].into())).unwrap();
    let err = upload.await.unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("upload abandoned"), "{err:#}");
    let (status, _, _, blobs) = job_state(&pool, id).await;
    assert_eq!((status.as_str(), blobs), ("failed", 0));
}

/// Search writes are serialised: concurrent rebuilds and syncs all succeed,
/// leave the index complete and no scratch index behind (including one left
/// by an interrupted rebuild).
#[tokio::test]
async fn concurrent_search_rebuilds() {
    let Some(c) = setup(None).await else { return };
    let Some(m) = c.svc.state.meili.clone() else {
        return;
    };
    c.ingest_bytes(encode(&bundle(), Format::Json)).await;
    let meili_url = nfi_testenv::services().meili_url.clone().unwrap();
    let http = reqwest::Client::new();
    let indexes = || async {
        let r: Value = http
            .get(format!("{meili_url}/indexes?limit=1000"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        r["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|i| i["uid"].as_str())
            .filter(|uid| *uid == m.index || uid.starts_with(&format!("{}_rebuild", m.index)))
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    // A scratch index left by an interrupted rebuild (of an older version).
    let r = http
        .post(format!("{meili_url}/indexes"))
        .json(&json!({"uid": format!("{}_rebuild", m.index)}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let pool = &c.svc.state.pool;
    let attrs: std::collections::BTreeSet<String> = ["hello".to_string(), "curl".to_string()].into();
    let (a, b, s1, s2) = tokio::join!(
        m.reindex(pool),
        m.reindex(pool),
        m.sync_attrs(pool, &attrs),
        m.sync_attrs(pool, &attrs),
    );
    assert_eq!((a.unwrap(), b.unwrap(), s1.unwrap(), s2.unwrap()), (4, 4, 2, 2));
    let mut left = Vec::new();
    for _ in 0..50 {
        left = indexes().await;
        if left == std::slice::from_ref(&m.index) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(left, std::slice::from_ref(&m.index));
    let (_, r): (u16, SearchResponse) = c.get("/v1/search?q=transfer").await;
    assert_eq!(r.hits.len(), 1);
}

/// Foreign keys that cascade from closures are indexed.
#[tokio::test]
async fn missing_paths_closure_is_indexed() {
    let Some(pool) = job_pool().await else { return };
    let def: Option<String> =
        sqlx::query_scalar("SELECT indexdef FROM pg_indexes WHERE indexname = 'missing_paths_closure'")
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert!(def.unwrap().contains("(closure_id)"));
}

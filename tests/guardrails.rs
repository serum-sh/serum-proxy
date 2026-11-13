//! Method, allowlist, body-size, and concurrency-cap guardrails.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{Router, body::Body, http::StatusCode, response::IntoResponse, routing::get};
use bytes::Bytes;
use serum_proxy::{
    config::{ServiceConfig, ServiceKind},
    egress::EgressBuffer,
    envelope::{ErrorCode, HttpMethod, ProtocolV1, RequestFrame},
    error::ProxyError,
    executor::{Executor, MAX_RESPONSE_BYTES, ResolvedService, build_client},
    guardrails::{Guardrails, default_allowlist},
};
use tokio::net::TcpListener;

fn svc(base_url: &str, kind: ServiceKind, max_concurrent: usize) -> ServiceConfig {
    ServiceConfig {
        kind,
        base_url: base_url.into(),
        headers: BTreeMap::new(),
        max_concurrent_requests: max_concurrent,
        path_allowlist: None,
        tls_ca_cert: None,
        auth_token_file: None,
    }
}

fn make_request(target: &str, method: HttpMethod, path: &str) -> RequestFrame {
    RequestFrame {
        protocol_version: ProtocolV1,
        request_id: "019e2d7a444e70ef8250f75c26cc5f1d".into(),
        target_service: target.into(),
        method,
        path: path.into(),
        query: None,
        headers: None,
        body: None,
    }
}

#[test]
fn deserialize_rejects_delete_method() {
    let raw = r#"{
      "type": "request",
      "protocol_version": 1,
      "request_id": "019e2d7a444e70ef8250f75c26cc5f1d",
      "target_service": "prometheus",
      "method": "DELETE",
      "path": "/api/v1/query"
    }"#;
    let r: Result<serum_proxy::envelope::Frame, _> = serde_json::from_str(raw);
    assert!(
        r.is_err(),
        "DELETE method should fail at the envelope layer"
    );
}

#[tokio::test]
async fn rejects_path_outside_prometheus_allowlist() {
    // Admission rejects an admin path by value: no executor, no backend.
    let g =
        Guardrails::from_service(&svc("http://127.0.0.1:1", ServiceKind::Prometheus, 4)).unwrap();
    // CeilingPermit is not Debug, so match on the result rather than expect_err.
    assert!(
        matches!(
            g.admit(&HttpMethod::Post, "/api/v1/admin/tsdb/snapshot")
                .await,
            Err(ProxyError::AllowlistDeny { .. })
        ),
        "admin path must be denied"
    );
}

#[tokio::test]
async fn rejects_elasticsearch_bulk() {
    let g = Guardrails::from_service(&svc("http://127.0.0.1:1", ServiceKind::Elasticsearch, 4))
        .unwrap();
    assert!(
        matches!(
            g.admit(&HttpMethod::Post, "/_bulk").await,
            Err(ProxyError::AllowlistDeny { .. })
        ),
        "_bulk must be denied"
    );
}

#[tokio::test]
async fn allows_per_index_search() {
    // Path under the wildcard `/*/_search` is admitted and hands back a permit;
    // no outbound call needed to verify the verdict.
    let g = Guardrails::from_service(&svc("http://127.0.0.1:1", ServiceKind::Elasticsearch, 4))
        .unwrap();
    let permit = g.admit(&HttpMethod::Post, "/logs-2025/_search").await;
    assert!(permit.is_ok(), "per-index search path should be admitted");
}

#[tokio::test]
async fn forbidden_path_beats_concurrency_and_takes_no_permit() {
    // Precedence: path (403) is checked before concurrency (429). A forbidden
    // path is denied even with the only slot full, and must not consume it.
    let g = Guardrails::from_service(&svc("http://127.0.0.1:1", ServiceKind::Elasticsearch, 1))
        .unwrap();
    let _slot = g
        .admit(&HttpMethod::Post, "/logs-2025/_search")
        .await
        .expect("fills the only slot");
    assert_eq!(g.in_flight_count(), 1);

    assert!(
        matches!(
            g.admit(&HttpMethod::Post, "/_bulk").await,
            Err(ProxyError::AllowlistDeny { .. })
        ),
        "forbidden path at the ceiling is still a 403"
    );
    assert_eq!(
        g.in_flight_count(),
        1,
        "denied path must not count against the cap"
    );
}

#[tokio::test]
async fn kubernetes_admits_in_scope_and_rejects_out_of_scope() {
    // The Kubernetes allowlist admits the namespaced pods and events list paths
    // and rejects anything outside that resource-anchored set.
    let g =
        Guardrails::from_service(&svc("http://127.0.0.1:1", ServiceKind::Kubernetes, 4)).unwrap();

    assert!(
        g.admit(&HttpMethod::Get, "/api/v1/namespaces/payments/pods")
            .await
            .is_ok(),
        "namespaced pods list must be admitted"
    );
    assert!(
        g.admit(&HttpMethod::Get, "/api/v1/namespaces/payments/events")
            .await
            .is_ok(),
        "namespaced events list must be admitted"
    );

    // A named-object sub-path is out of scope: the resource glob has no trailing
    // wildcard, so a single segment past `pods` is denied.
    assert!(
        matches!(
            g.admit(&HttpMethod::Get, "/api/v1/namespaces/payments/pods/web-0")
                .await,
            Err(ProxyError::AllowlistDeny { .. })
        ),
        "a named pod path must be denied"
    );
    // A cluster-scoped resource (out of scope for cut 1) is denied.
    assert!(
        matches!(
            g.admit(&HttpMethod::Get, "/api/v1/nodes").await,
            Err(ProxyError::AllowlistDeny { .. })
        ),
        "cluster-scoped nodes path must be denied"
    );
}

#[tokio::test]
async fn kubernetes_rejects_non_get_method() {
    // POST to an allowlisted Kubernetes path is rejected (405)
    let g =
        Guardrails::from_service(&svc("http://127.0.0.1:1", ServiceKind::Kubernetes, 4)).unwrap();

    assert!(
        matches!(
            g.admit(&HttpMethod::Post, "/api/v1/namespaces/payments/pods")
                .await,
            Err(ProxyError::MethodNotAllowed { .. })
        ),
        "POST to the Kubernetes kind must be rejected before forwarding"
    );
    // GET on the same path is admitted
    assert!(
        g.admit(&HttpMethod::Get, "/api/v1/namespaces/payments/pods")
            .await
            .is_ok(),
        "GET to an in-scope Kubernetes path must still be admitted"
    );
}

#[tokio::test]
async fn kubernetes_non_get_yields_405_frame_without_forwarding() {
    //end-to-end through the executor, POST check for 405_METHOD.
    // base_url is a refused port, so a forwarded dial would return 502 BACKEND_UNREACHABLE.
    let s = svc("http://127.0.0.1:1", ServiceKind::Kubernetes, 4);
    let resolved = Arc::new(ResolvedService::from_config(&s).unwrap());
    let exec = Executor::new(build_client(), EgressBuffer::new(64));
    let req = make_request("k8s", HttpMethod::Post, "/api/v1/namespaces/payments/pods");
    let resp = exec.execute(req, &resolved).await;

    let err = resp.error.as_ref().expect("expected a proxy error frame");
    assert_eq!(
        err.code,
        ErrorCode::MethodNotAllowed,
        "a rejected method must be 405"
    );
    assert!(
        err.message.contains("POST") && err.message.contains("not allowed for this path"),
        "405 message must name the method and read as not-allowed: {}",
        err.message
    );
    assert!(!err.retryable, "a 405 is not retryable");

    // Assert the wire form, not just the in-memory enum: the serialized frame
    // carries the `405_METHOD` code string serum maps to its typed exception.
    let wire = serde_json::to_string(&resp).expect("response frame serializes");
    assert!(
        wire.contains("405_METHOD"),
        "serialized error frame must carry the 405_METHOD wire code: {wire}"
    );
}

const KINDS: [ServiceKind; 3] = [
    ServiceKind::Prometheus,
    ServiceKind::Elasticsearch,
    ServiceKind::Kubernetes,
];

fn guardrails(kind: ServiceKind, path_allowlist: Option<&[&str]>) -> Guardrails {
    let mut s = svc("http://127.0.0.1:1", kind, 4);
    s.path_allowlist = path_allowlist.map(|paths| paths.iter().map(|p| p.to_string()).collect());
    Guardrails::from_service(&s).unwrap()
}

fn other_method(method: &HttpMethod) -> HttpMethod {
    match method {
        HttpMethod::Get => HttpMethod::Post,
        HttpMethod::Post => HttpMethod::Get,
    }
}

#[tokio::test]
async fn each_entry_allows_its_method_and_gives_405_for_the_other() {
    for kind in KINDS {
        let g = guardrails(kind, None);
        for (method, pattern) in default_allowlist(kind) {
            let path = pattern.replace('*', "x");
            assert!(
                g.admit(method, &path).await.is_ok(),
                "{kind:?}: {method:?} {path} must be admitted"
            );
            let other = other_method(method);
            assert!(
                matches!(
                    g.admit(&other, &path).await,
                    Err(ProxyError::MethodNotAllowed { .. })
                ),
                "{kind:?}: {other:?} {path} must be 405"
            );
        }
    }
}

#[tokio::test]
async fn paths_outside_the_builtin_list_give_403() {
    let outside: &[(ServiceKind, &str)] = &[
        (ServiceKind::Prometheus, "/api/v1/labels"),
        (ServiceKind::Prometheus, "/api/v1/targets"),
        (ServiceKind::Prometheus, "/api/v1/rules"),
        (ServiceKind::Prometheus, "/api/v1/alerts"),
        (ServiceKind::Elasticsearch, "/_search"),
        (ServiceKind::Elasticsearch, "/_count"),
        (ServiceKind::Elasticsearch, "/logs-2025/_count"),
        (ServiceKind::Elasticsearch, "/_cat/nodes"),
        (ServiceKind::Elasticsearch, "/_mapping"),
        (ServiceKind::Elasticsearch, "/_cluster/health"),
    ];
    for (kind, path) in outside {
        let g = guardrails(*kind, None);
        for method in [HttpMethod::Get, HttpMethod::Post] {
            assert!(
                matches!(
                    g.admit(&method, path).await,
                    Err(ProxyError::AllowlistDeny { .. })
                ),
                "{kind:?}: {method:?} {path} must be 403"
            );
        }
    }
}

#[tokio::test]
async fn post_mapping_gives_405() {
    // `POST /{index}/_mapping` is the Elasticsearch put mapping API: a write.
    let g = guardrails(ServiceKind::Elasticsearch, None);
    assert!(
        matches!(
            g.admit(&HttpMethod::Post, "/logs-2025/_mapping").await,
            Err(ProxyError::MethodNotAllowed { .. })
        ),
        "POST /*/_mapping must be 405"
    );
    assert!(
        g.admit(&HttpMethod::Get, "/logs-2025/_mapping")
            .await
            .is_ok(),
        "GET /*/_mapping must be admitted"
    );
}

#[test]
fn each_kubernetes_entry_is_get() {
    assert!(
        default_allowlist(ServiceKind::Kubernetes)
            .iter()
            .all(|(method, _)| *method == HttpMethod::Get),
        "the proxy must keep Kubernetes read-only"
    );
}

#[tokio::test]
async fn star_matches_one_segment_for_all_kinds() {
    let cases: &[(ServiceKind, HttpMethod, &str)] = &[
        (
            ServiceKind::Prometheus,
            HttpMethod::Get,
            "/api/v1/label/a/b/values",
        ),
        (
            ServiceKind::Elasticsearch,
            HttpMethod::Get,
            "/_cat/indices/a/b",
        ),
        (ServiceKind::Elasticsearch, HttpMethod::Get, "/a/b/_mapping"),
        (ServiceKind::Elasticsearch, HttpMethod::Post, "/a/b/_search"),
        (
            ServiceKind::Kubernetes,
            HttpMethod::Get,
            "/api/v1/namespaces/a/b/pods",
        ),
    ];
    for (kind, method, path) in cases {
        let g = guardrails(*kind, None);
        assert!(
            matches!(
                g.admit(method, path).await,
                Err(ProxyError::AllowlistDeny { .. })
            ),
            "{kind:?}: `*` must not cross `/` in {path}"
        );
    }
}

#[tokio::test]
async fn path_allowlist_makes_the_builtin_list_shorter() {
    // `/api/v1/admin/*` is not in the built-in list, so it allows nothing.
    let g = guardrails(
        ServiceKind::Prometheus,
        Some(&["/api/v1/query", "/api/v1/admin/*"]),
    );
    assert!(
        g.admit(&HttpMethod::Get, "/api/v1/query").await.is_ok(),
        "a path in both lists must be admitted"
    );
    assert!(
        matches!(
            g.admit(&HttpMethod::Get, "/api/v1/query_range").await,
            Err(ProxyError::AllowlistDeny { .. })
        ),
        "a built-in path that is not in path_allowlist must be 403"
    );
    assert!(
        matches!(
            g.admit(&HttpMethod::Get, "/api/v1/admin/tsdb").await,
            Err(ProxyError::AllowlistDeny { .. })
        ),
        "a path_allowlist path that is not in the built-in list must be 403"
    );
    assert!(
        matches!(
            g.admit(&HttpMethod::Post, "/api/v1/query").await,
            Err(ProxyError::MethodNotAllowed { .. })
        ),
        "path_allowlist must not add a method"
    );
}

#[test]
fn path_allowlist_entries_that_overlap_no_builtin_entry_are_unused() {
    // `/api/v1/*` is wider and `/api/v1/label/job/values` is narrower than a built-in entry.
    // `/api/v1/label/job/*` has its `*` in a different segment than `/api/v1/label/*/values`.
    let g = guardrails(
        ServiceKind::Prometheus,
        Some(&[
            "/api/v1/query",
            "/api/v1/*",
            "/api/v1/label/job/values",
            "/api/v1/label/job/*",
            "/api/v1/labels",
        ]),
    );
    assert_eq!(g.unused_path_allowlist, ["/api/v1/labels"]);
}

async fn spawn_oversize_backend() -> (String, tokio::task::JoinHandle<()>) {
    async fn oversize() -> impl IntoResponse {
        // 6 MiB payload, well over the 5 MiB cap.
        let bytes = Bytes::from(vec![0u8; 6 * 1024 * 1024]);
        (StatusCode::OK, Body::from(bytes))
    }
    let app = Router::new().route("/api/v1/query", get(oversize));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, handle)
}

#[tokio::test]
async fn body_size_cap_triggers_at_5mb() {
    let (url, handle) = spawn_oversize_backend().await;
    let s = svc(&url, ServiceKind::Prometheus, 4);
    let resolved = Arc::new(ResolvedService::from_config(&s).unwrap());
    let exec = Executor::new(build_client(), EgressBuffer::new(64));
    let req = make_request("p", HttpMethod::Get, "/api/v1/query");
    let resp = exec.execute(req, &resolved).await;
    let err = resp.error.expect("expected error frame");
    assert_eq!(err.code, ErrorCode::BodyLimitExceeded);
    assert!(err.message.contains(&MAX_RESPONSE_BYTES.to_string()));
    handle.abort();
}

#[tokio::test]
async fn concurrency_ceiling_rejects_above_2x() {
    // max_concurrent=1, ceiling=2: one active + one queued = 2; the third is 429.
    let s = svc("http://127.0.0.1:1", ServiceKind::Prometheus, 1);
    let g = Arc::new(Guardrails::from_service(&s).unwrap());

    // Take the only active permit (in_flight=1).
    let p1 = g
        .admit(&HttpMethod::Get, "/api/v1/query")
        .await
        .expect("first permit");
    assert_eq!(g.in_flight_count(), 1);

    // Queue a second one (in_flight=2, semaphore-blocked).
    let g2 = g.clone();
    let waiter = tokio::spawn(async move {
        let _p = g2.admit(&HttpMethod::Get, "/api/v1/query").await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(g.in_flight_count(), 2);

    // Third arrival exceeds the ceiling -> 429.
    let result = g.admit(&HttpMethod::Get, "/api/v1/query").await;
    assert!(
        matches!(result, Err(ProxyError::ConcurrencyExceeded { .. })),
        "expected ConcurrencyExceeded at ceiling"
    );
    drop(p1);
    let _ = waiter.await;
}

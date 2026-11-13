//! A path that URL parsing changes, or that has an encoded separator, gets 403 and no backend
//! call.

use std::collections::BTreeMap;

use serum_proxy::{
    config::{ServiceConfig, ServiceKind},
    egress::EgressBuffer,
    envelope::{ErrorCode, HttpMethod, ProtocolV1, RequestFrame},
    error::ProxyError,
    executor::{Executor, ResolvedService, build_client, build_url},
};

/// Refused port: a request that gets past the guardrails gives 502, not 403.
const REFUSED: &str = "http://127.0.0.1:1";

fn svc(base_url: &str, kind: ServiceKind) -> ServiceConfig {
    ServiceConfig {
        kind,
        base_url: base_url.into(),
        headers: BTreeMap::new(),
        max_concurrent_requests: 4,
        path_allowlist: None,
        tls_ca_cert: None,
        auth_token_file: None,
    }
}

fn make_request(method: HttpMethod, path: &str) -> RequestFrame {
    RequestFrame {
        protocol_version: ProtocolV1,
        request_id: "019e2d7a444e70ef8250f75c26cc5f1d".into(),
        target_service: "svc".into(),
        method,
        path: path.into(),
        query: None,
        headers: None,
        body: None,
    }
}

async fn execute(kind: ServiceKind, method: HttpMethod, path: &str) -> ErrorCode {
    let resolved = ResolvedService::from_config(&svc(REFUSED, kind)).unwrap();
    let exec = Executor::new(build_client(), EgressBuffer::new(64));
    let resp = exec.execute(make_request(method, path), &resolved).await;
    resp.error.expect("expected an error frame").code
}

/// Each case: kind, method and a path that the proxy must deny.
const TRAVERSALS: &[(ServiceKind, HttpMethod, &str)] = &[
    (
        ServiceKind::Elasticsearch,
        HttpMethod::Post,
        "/_cat/../_bulk",
    ),
    (
        ServiceKind::Kubernetes,
        HttpMethod::Get,
        "/api/v1/namespaces/../pods",
    ),
    (
        ServiceKind::Kubernetes,
        HttpMethod::Get,
        "/api/v1/namespaces/kube-system\\secrets?/pods",
    ),
    (
        ServiceKind::Elasticsearch,
        HttpMethod::Post,
        "/logs/_delete_by_query#/_search",
    ),
    (
        ServiceKind::Prometheus,
        HttpMethod::Get,
        "/api/v1/label/../status/values",
    ),
    (
        ServiceKind::Prometheus,
        HttpMethod::Get,
        "/api/v1/label/%2e%2e/status/values",
    ),
    (
        ServiceKind::Prometheus,
        HttpMethod::Get,
        "/api/v1/label/a\\b/values",
    ),
    (
        ServiceKind::Prometheus,
        HttpMethod::Get,
        "/api/v1/label/a?/values",
    ),
    (
        ServiceKind::Prometheus,
        HttpMethod::Get,
        "/api/v1/label/a#/values",
    ),
    (
        ServiceKind::Elasticsearch,
        HttpMethod::Post,
        "/_cat/%2E%2e/_bulk",
    ),
    (ServiceKind::Elasticsearch, HttpMethod::Get, "/_cat/a\\b"),
    (
        ServiceKind::Elasticsearch,
        HttpMethod::Post,
        "/logs?/_search",
    ),
    (
        ServiceKind::Kubernetes,
        HttpMethod::Get,
        "/api/v1/namespaces/%2e%2e/pods",
    ),
    (
        ServiceKind::Kubernetes,
        HttpMethod::Get,
        "/api/v1/namespaces/a\\b/pods",
    ),
    (
        ServiceKind::Kubernetes,
        HttpMethod::Get,
        "/api/v1/namespaces/a?/pods",
    ),
    (
        ServiceKind::Kubernetes,
        HttpMethod::Get,
        "/api/v1/namespaces/a#/pods",
    ),
    // Encoded separators, in both letter cases.
    (
        ServiceKind::Kubernetes,
        HttpMethod::Get,
        "/api/v1/namespaces/kube-system%2Fsecrets/pods",
    ),
    (
        ServiceKind::Kubernetes,
        HttpMethod::Get,
        "/api/v1/namespaces/kube-system%2fservices%2fdns:53%2fproxy/pods",
    ),
    (
        ServiceKind::Kubernetes,
        HttpMethod::Get,
        "/api/v1/namespaces/a%5Cb/pods",
    ),
    (
        ServiceKind::Prometheus,
        HttpMethod::Get,
        "/api/v1/label/a%2Fb/values",
    ),
    // No leading `/`.
    (
        ServiceKind::Prometheus,
        HttpMethod::Get,
        "@127.0.0.2/api/v1/query",
    ),
    // Empty segment, also at the end.
    (
        ServiceKind::Kubernetes,
        HttpMethod::Get,
        "/api/v1/namespaces//pods",
    ),
    (ServiceKind::Elasticsearch, HttpMethod::Post, "//_search"),
    (
        ServiceKind::Prometheus,
        HttpMethod::Get,
        "/api/v1/label//values",
    ),
    (
        ServiceKind::Elasticsearch,
        HttpMethod::Get,
        "/_cat/indices/",
    ),
];

const CLEAN: &[(ServiceKind, HttpMethod, &str)] = &[
    (
        ServiceKind::Prometheus,
        HttpMethod::Get,
        "/api/v1/label/job/values",
    ),
    (
        ServiceKind::Elasticsearch,
        HttpMethod::Post,
        "/logs-*/_search",
    ),
    (
        ServiceKind::Elasticsearch,
        HttpMethod::Get,
        "/_cat/indices/logs-2025",
    ),
    (
        ServiceKind::Kubernetes,
        HttpMethod::Get,
        "/api/v1/namespaces/payments/pods",
    ),
];

#[tokio::test]
async fn traversal_paths_get_403_and_no_backend_call() {
    for (kind, method, path) in TRAVERSALS {
        assert_eq!(
            execute(*kind, method.clone(), path).await,
            ErrorCode::AllowlistDeny,
            "{kind:?} {path} must be denied before any backend call"
        );
    }
}

#[tokio::test]
async fn clean_paths_reach_the_backend() {
    for (kind, method, path) in CLEAN {
        assert_eq!(
            execute(*kind, method.clone(), path).await,
            ErrorCode::BackendUnreachable,
            "{kind:?} {path} must pass the guardrails and dial the backend"
        );
    }
}

#[test]
fn base_path_is_kept_and_only_the_part_after_it_is_compared() {
    for base in ["http://h:9090/prometheus", "http://h:9090/prometheus/"] {
        let cfg = svc(base, ServiceKind::Prometheus);
        let url = build_url(&cfg, "/api/v1/query", None).unwrap();
        assert_eq!(url.as_str(), "http://h:9090/prometheus/api/v1/query");

        // Resolves to `/api/v1/query` outside the base path.
        assert!(matches!(
            build_url(&cfg, "/../api/v1/query", None),
            Err(ProxyError::AllowlistDeny { .. })
        ));
    }
}

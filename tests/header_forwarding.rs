//! Request headers from serum that reach the backend.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::{Router, http::HeaderMap, routing::get};
use serum_proxy::{
    config::{ServiceConfig, ServiceKind},
    egress::EgressBuffer,
    envelope::{HttpMethod, ProtocolV1, RequestFrame},
    executor::{Executor, ResolvedService, build_client},
};
use tokio::net::TcpListener;

const PATH: &str = "/api/v1/query";

fn to_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Send one request with `headers` from serum to a Prometheus service with `config_headers`.
/// Returns the headers that the backend received.
async fn backend_headers(headers: &[(&str, &str)], config_headers: &[(&str, &str)]) -> HeaderMap {
    let captured: Arc<Mutex<HeaderMap>> = Arc::default();
    let captured_handler = captured.clone();
    let app = Router::new().route(
        PATH,
        get(move |headers: HeaderMap| {
            let captured = captured_handler.clone();
            async move {
                *captured.lock().unwrap() = headers;
                "ok"
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let svc = ServiceConfig {
        kind: ServiceKind::Prometheus,
        base_url: format!("http://{addr}"),
        headers: to_map(config_headers),
        max_concurrent_requests: 4,
        path_allowlist: None,
        tls_ca_cert: None,
        auth_token_file: None,
    };
    let resolved = ResolvedService::from_config(&svc).expect("service resolves");
    let req = RequestFrame {
        protocol_version: ProtocolV1,
        request_id: "019e2d7a444e70ef8250f75c26cc5f1d".into(),
        target_service: "prometheus".into(),
        method: HttpMethod::Get,
        path: PATH.into(),
        query: None,
        headers: Some(to_map(headers)),
        body: None,
    };
    let exec = Executor::new(build_client(), EgressBuffer::new(64));
    let resp = exec.execute(req, &resolved).await;
    handle.abort();

    assert_eq!(resp.status_code, Some(200), "request reached the backend");
    captured.lock().unwrap().clone()
}

#[tokio::test]
async fn forwards_only_accept_and_content_type() {
    let seen = backend_headers(
        &[
            ("Accept", "application/json"),
            ("content-type", "application/json"),
            ("Host", "other-backend.internal"),
            ("X-Scope-OrgID", "other-tenant"),
            ("Impersonate-User", "admin"),
        ],
        &[],
    )
    .await;

    assert_eq!(seen["accept"], "application/json");
    assert_eq!(seen["content-type"], "application/json");
    assert_ne!(seen["host"], "other-backend.internal");
    for name in ["x-scope-orgid", "impersonate-user"] {
        assert!(
            !seen.contains_key(name),
            "{name} must not reach the backend"
        );
    }
}

#[tokio::test]
async fn config_header_replaces_header_from_serum() {
    let seen = backend_headers(
        &[("accept", "text/plain")],
        &[("Accept", "application/json")],
    )
    .await;

    let accept: Vec<_> = seen.get_all("accept").iter().collect();
    assert_eq!(accept, ["application/json"], "only the config value");
}

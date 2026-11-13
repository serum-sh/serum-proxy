//! `auth_token_file`: token caching, re-read after the TTL, and injection after
//! the incoming Authorization is stripped.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{Router, http::HeaderMap, routing::get};
use serum_proxy::{
    config::{Config, ServiceConfig, ServiceKind},
    egress::EgressBuffer,
    envelope::{HttpMethod, ProtocolV1, RequestFrame},
    executor::{Executor, ResolvedService, build_client, build_token_source_with_ttl},
};
use tokio::net::TcpListener;

#[test]
fn serves_cached_token_within_ttl_then_rereads_after() {
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, "token-v1\n").unwrap();

    // Short TTL so the test crosses the re-read boundary quickly.
    let yaml = format!(
        r#"
proxy:
  serum_wss_url: "wss://gateway.example.com/v1/proxy"
services:
  - type: "kubernetes"
    base_url: "https://kubernetes.default.svc"
    auth_token_file: "{}"
"#,
        token_path.display()
    );
    let env: HashMap<&str, &str> = HashMap::new();
    let cfg =
        Config::from_yaml_str(&yaml, |k| env.get(k).map(|s| s.to_string())).expect("config loads");
    let ttl = Duration::from_millis(200);
    let source = build_token_source_with_ttl(&cfg.services[0], ttl)
        .expect("token source builds")
        .expect("token source present when auth_token_file is set");

    // The trailing newline is trimmed.
    assert_eq!(source.token(), "token-v1");

    // Rotate on disk; still cached within the TTL.
    std::fs::write(&token_path, "token-v2\n").unwrap();
    assert_eq!(source.token(), "token-v1", "cached within the TTL");

    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(source.token(), "token-v2", "re-read after the TTL");
}

#[tokio::test]
async fn injects_bearer_after_stripping_incoming_authorization() {
    // Backend records the Authorization it received.
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured_handler = captured.clone();
    let app = Router::new().route(
        "/api/v1/namespaces/default/pods",
        get(move |headers: HeaderMap| {
            let captured = captured_handler.clone();
            async move {
                let auth = headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                *captured.lock().unwrap() = auth;
                "ok"
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let backend = format!("http://{addr}");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, "token-from-file\n").unwrap();

    // Built literally, not via config: the backend is plain http and config
    // validation requires https.
    let svc = ServiceConfig {
        kind: ServiceKind::Kubernetes,
        base_url: backend,
        headers: BTreeMap::new(),
        max_concurrent_requests: 4,
        path_allowlist: None,
        tls_ca_cert: None,
        auth_token_file: Some(token_path.display().to_string()),
    };
    let resolved = ResolvedService::from_config(&svc).expect("service resolves");

    let exec = Executor::new(build_client(), EgressBuffer::new(64));
    let req = RequestFrame {
        protocol_version: ProtocolV1,
        request_id: "019e2d7a444e70ef8250f75c26cc5f1d".into(),
        target_service: "kubernetes".into(),
        method: HttpMethod::Get,
        path: "/api/v1/namespaces/default/pods".into(),
        query: None,
        // Incoming Authorization that must be stripped, not forwarded.
        headers: Some(BTreeMap::from([(
            "Authorization".into(),
            "Bearer incoming-should-be-stripped".into(),
        )])),
        body: None,
    };
    let resp = exec.execute(req, &resolved).await;
    handle.abort();

    assert_eq!(resp.status_code, Some(200), "request reached the backend");
    let seen = captured.lock().unwrap().clone();
    assert_eq!(
        seen.as_deref(),
        Some("Bearer token-from-file"),
        "backend sees the injected file token, not the stripped incoming one"
    );
}

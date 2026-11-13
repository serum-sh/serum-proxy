//! Inbound dispatch decisions, by value through `session::decide`, with no live
//! WebSocket.

use std::collections::BTreeMap;
use std::sync::Arc;

use serum_proxy::{
    config::{ServiceConfig, ServiceKind},
    envelope::{
        ErrorCode, ErrorFrameBody, Frame, FrameError, HeartbeatFrame, HelloFrame, HttpMethod,
        ProtocolV1, RequestFrame, ResponseFrame,
    },
    executor::ResolvedService,
    session::{Action, ServiceRegistry, decide},
};

/// 32 hex chars: a well-formed UUIDv7 `request_id` (see `is_valid_request_id`).
const VALID_REQUEST_ID: &str = "0190b2c3d4e5f6a7b8c9d0e1f2a3b4c5";

/// A registry holding one minimal service. Plain-http `base_url` skips config's
/// https validation; `from_config` exercises the real assembly path.
fn registry_with(key: &str) -> ServiceRegistry {
    let cfg = ServiceConfig {
        kind: ServiceKind::Prometheus,
        base_url: "http://127.0.0.1:1".into(),
        headers: BTreeMap::new(),
        max_concurrent_requests: 4,
        path_allowlist: None,
        tls_ca_cert: None,
        auth_token_file: None,
    };
    let resolved = ResolvedService::from_config(&cfg).expect("service resolves");
    let mut registry = ServiceRegistry::new();
    registry.insert(key.into(), Arc::new(resolved));
    registry
}

fn request(request_id: &str, target_service: &str) -> Frame {
    Frame::Request(RequestFrame {
        protocol_version: ProtocolV1,
        request_id: request_id.into(),
        target_service: target_service.into(),
        method: HttpMethod::Get,
        path: "/api/v1/query".into(),
        query: None,
        headers: None,
        body: None,
    })
}

#[test]
fn valid_request_to_known_service_forwards() {
    let registry = registry_with("prometheus");
    match decide(Ok(request(VALID_REQUEST_ID, "prometheus")), &registry) {
        Action::Forward(req, service) => {
            assert_eq!(req.request_id, VALID_REQUEST_ID);
            assert_eq!(service.config.kind, ServiceKind::Prometheus);
        }
        _ => panic!("a valid request to a known service must forward"),
    }
}

#[test]
fn unknown_service_rejects_as_404_not_403() {
    // An unknown target_service must surface as 404_UNKNOWN_SERVICE.
    // The original bug emitted 403_ALLOWLIST here; lock the path from a missing
    // registry entry to the rejection verdict.
    let registry = ServiceRegistry::new();
    match decide(Ok(request(VALID_REQUEST_ID, "nope")), &registry) {
        Action::Reject(resp) => {
            let err = resp.error.expect("rejection carries a typed error");
            assert_eq!(err.code, ErrorCode::UnknownService);
        }
        _ => panic!("an unknown service must reject with a response"),
    }
}

#[test]
fn invalid_request_id_rejects() {
    // A non-UUIDv7 request_id is rejected before any service lookup.
    let registry = registry_with("prometheus");
    match decide(Ok(request("not-a-uuid", "prometheus")), &registry) {
        Action::Reject(resp) => {
            let err = resp.error.expect("rejection carries a typed error");
            assert_eq!(err.code, ErrorCode::FrameParseFailed);
            assert_eq!(resp.request_id, "not-a-uuid");
        }
        _ => panic!("a bad request_id must reject with a response"),
    }
}

#[test]
fn parse_failure_emits_protocol_error() {
    let registry = ServiceRegistry::new();
    match decide(Err(anyhow::anyhow!("trailing garbage")), &registry) {
        Action::Emit(ErrorFrameBody { error, .. }) => {
            assert_eq!(error.code, ErrorCode::FrameParseFailed);
            assert!(
                error.message.contains("trailing garbage"),
                "the parse error message is carried through: {}",
                error.message
            );
        }
        _ => panic!("a parse failure must emit an out-of-band error"),
    }
}

#[test]
fn non_actionable_frames_are_ignored() {
    let registry = ServiceRegistry::new();

    let heartbeat = Frame::Heartbeat(HeartbeatFrame {
        protocol_version: ProtocolV1,
        uptime_seconds: 0,
        requests_handled_since_last: 0,
    });
    assert!(matches!(decide(Ok(heartbeat), &registry), Action::Ignore));

    let hello = Frame::Hello(HelloFrame {
        protocol_versions_supported: None,
        protocol_version: Some(1),
        proxy_build_version: None,
    });
    assert!(matches!(decide(Ok(hello), &registry), Action::Ignore));

    let response = Frame::Response(ResponseFrame::error(
        VALID_REQUEST_ID.into(),
        FrameError {
            code: ErrorCode::UnknownService,
            message: "stray".into(),
            retryable: false,
        },
    ));
    assert!(matches!(decide(Ok(response), &registry), Action::Ignore));

    let error = Frame::Error(ErrorFrameBody {
        protocol_version: ProtocolV1,
        error: FrameError {
            code: ErrorCode::FrameParseFailed,
            message: "stray".into(),
            retryable: false,
        },
    });
    assert!(matches!(decide(Ok(error), &registry), Action::Ignore));
}

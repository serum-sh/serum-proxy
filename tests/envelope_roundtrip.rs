//! JSON envelope round-trip tests for the v1 frame types. The canonical cross-repo
//! vectors live in `tests/protocol_vectors.rs` (`docs/protocol-vectors.json`); these are
//! additional hand-written Rust-side checks.

use std::collections::BTreeMap;

use serum_proxy::envelope::{
    ErrorCode, ErrorFrameBody, Frame, FrameError, HeartbeatFrame, HelloFrame, HttpMethod,
    ProtocolV1, QueryValue, RequestFrame, ResponseFrame, is_valid_request_id,
};

fn parse(s: &str) -> Frame {
    serde_json::from_str(s).expect("frame parses")
}

fn json(f: &Frame) -> serde_json::Value {
    serde_json::to_value(f).expect("frame serializes")
}

#[test]
fn request_frame_roundtrip() {
    let raw = r#"{
      "type": "request",
      "protocol_version": 1,
      "request_id": "019e2d7a444e70ef8250f75c26cc5f1d",
      "target_service": "prometheus",
      "method": "GET",
      "path": "/api/v1/query",
      "query": {"query": "up", "time": "1700000000"},
      "headers": {"Accept": "application/json"},
      "body": null
    }"#;
    let frame = parse(raw);
    match &frame {
        Frame::Request(r) => {
            assert_eq!(r.request_id, "019e2d7a444e70ef8250f75c26cc5f1d");
            assert_eq!(r.target_service, "prometheus");
            assert_eq!(r.method, HttpMethod::Get);
            assert_eq!(r.path, "/api/v1/query");
            let q = r.query.as_ref().unwrap();
            assert_eq!(q.get("query"), Some(&QueryValue::One("up".into())));
            assert_eq!(q.get("time"), Some(&QueryValue::One("1700000000".into())));
            assert_eq!(r.body, None);
        }
        _ => panic!("expected request frame"),
    }
    // Round-trip through value form.
    let value = json(&frame);
    assert_eq!(value["type"], "request");
    assert_eq!(value["protocol_version"], 1);
}

#[test]
fn list_query_values_round_trip() {
    let mut q: BTreeMap<String, QueryValue> = BTreeMap::new();
    q.insert(
        "match[]".to_string(),
        QueryValue::Many(vec!["a".into(), "b".into()]),
    );
    let frame = Frame::Request(RequestFrame {
        protocol_version: ProtocolV1,
        request_id: "019e2d7a444e70ef8250f75c26cc5f1d".into(),
        target_service: "prometheus".into(),
        method: HttpMethod::Get,
        path: "/api/v1/series".into(),
        query: Some(q),
        headers: None,
        body: None,
    });
    let s = serde_json::to_string(&frame).unwrap();
    let parsed: Frame = serde_json::from_str(&s).unwrap();
    assert_eq!(parsed, frame);
}

#[test]
fn response_frame_head_only_validator() {
    // Construct a permissively-parsed response with chunk_index > 0 and headers set.
    let raw = r#"{
      "type": "response",
      "protocol_version": 1,
      "request_id": "019e2d7a444e70ef8250f75c26cc5f1d",
      "is_streaming": true,
      "chunk_index": 1,
      "is_final": false,
      "status_code": 200,
      "headers": {"Content-Type": "application/json"},
      "body": "",
      "error": null,
      "redaction_summary": null
    }"#;
    let frame = parse(raw);
    if let Frame::Response(r) = frame {
        assert!(r.check_head_fields().is_err());
    } else {
        panic!("expected response");
    }
}

#[test]
fn response_frame_first_chunk_ok() {
    let raw = r#"{
      "type": "response",
      "protocol_version": 1,
      "request_id": "019e2d7a444e70ef8250f75c26cc5f1d",
      "is_streaming": false,
      "chunk_index": 0,
      "is_final": true,
      "status_code": 200,
      "headers": {"Content-Type": "application/json"},
      "body": "{}",
      "error": null,
      "redaction_summary": null
    }"#;
    let frame = parse(raw);
    if let Frame::Response(r) = frame {
        assert!(r.check_head_fields().is_ok());
        assert_eq!(r.status_code, Some(200));
        assert!(r.is_final);
    } else {
        panic!("expected response");
    }
}

#[test]
fn protocol_version_must_be_one() {
    let raw = r#"{
      "type": "heartbeat",
      "protocol_version": 2,
      "uptime_seconds": 5,
      "requests_handled_since_last": 0
    }"#;
    let r: Result<Frame, _> = serde_json::from_str(raw);
    assert!(r.is_err(), "expected protocol_version=2 to be rejected");
}

#[test]
fn hello_frame_roundtrip() {
    let raw = r#"{
      "type": "hello",
      "protocol_versions_supported": [1],
      "proxy_build_version": "0.1.0"
    }"#;
    let frame = parse(raw);
    match &frame {
        Frame::Hello(h) => {
            assert_eq!(h.protocol_versions_supported, Some(vec![1]));
            assert_eq!(h.proxy_build_version.as_deref(), Some("0.1.0"));
        }
        _ => panic!("expected hello"),
    }
    let v = json(&frame);
    assert_eq!(v["type"], "hello");

    // Hello is the one frame that accepts unknown fields, so a renamed field would
    // otherwise deserialize silently. Pin the full key set instead.
    let full = Frame::Hello(HelloFrame {
        protocol_versions_supported: Some(vec![1]),
        protocol_version: Some(1),
        proxy_build_version: Some("0.1.0".into()),
    });
    let full = json(&full);
    let keys: Vec<&str> = full
        .as_object()
        .expect("hello serializes to an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "protocol_version",
            "protocol_versions_supported",
            "proxy_build_version",
            "type",
        ]
    );
}

#[test]
fn hello_accepts_unknown_field() {
    // A peer of an unknown version may add fields; hello must still parse.
    let raw = r#"{
      "type": "hello",
      "protocol_version": 1,
      "heartbeat_interval_secs": 30
    }"#;
    match parse(raw) {
        Frame::Hello(h) => assert_eq!(h.protocol_version, Some(1)),
        _ => panic!("expected hello"),
    }
}

#[test]
fn heartbeat_frame_roundtrip() {
    let frame = Frame::Heartbeat(HeartbeatFrame {
        protocol_version: ProtocolV1,
        uptime_seconds: 12345,
        requests_handled_since_last: 17,
    });
    let v = json(&frame);
    assert_eq!(v["type"], "heartbeat");
    assert_eq!(v["protocol_version"], 1);
    assert_eq!(v["uptime_seconds"], 12345);
    assert_eq!(v["requests_handled_since_last"], 17);
    let parsed: Frame = serde_json::from_value(v).unwrap();
    assert_eq!(parsed, frame);
}

#[test]
fn error_frame_codes() {
    for (code_str, code) in [
        ("403_ALLOWLIST", ErrorCode::AllowlistDeny),
        ("405_METHOD", ErrorCode::MethodNotAllowed),
        ("413_BODY_LIMIT", ErrorCode::BodyLimitExceeded),
        ("429_CONCURRENT_REQUESTS", ErrorCode::ConcurrencyExceeded),
        ("504_TIMEOUT", ErrorCode::UpstreamTimeout),
        ("PROXY_DISCONNECTED", ErrorCode::ProxyDisconnected),
        ("VERSION_MISMATCH", ErrorCode::VersionMismatch),
        ("FRAME_PARSE_FAILED", ErrorCode::FrameParseFailed),
    ] {
        let frame = Frame::Error(ErrorFrameBody {
            protocol_version: ProtocolV1,
            error: FrameError {
                code,
                message: "x".into(),
                retryable: false,
            },
        });
        let v = json(&frame);
        assert_eq!(v["error"]["code"], code_str);
        let back: Frame = serde_json::from_value(v).unwrap();
        assert_eq!(back, frame);
    }
}

#[test]
fn unknown_type_is_rejected() {
    let raw = r#"{"type": "nope"}"#;
    let r: Result<Frame, _> = serde_json::from_str(raw);
    assert!(r.is_err());
}

#[test]
fn unknown_field_is_rejected() {
    let raw = r#"{
      "type": "heartbeat",
      "protocol_version": 1,
      "uptime_seconds": 0,
      "requests_handled_since_last": 0,
      "stowaway": true
    }"#;
    let r: Result<Frame, _> = serde_json::from_str(raw);
    assert!(r.is_err());
}

#[test]
fn reply_holds_single_chunk_success_shape() {
    let mut headers = BTreeMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    let f = ResponseFrame::reply(
        "019e2d7a444e70ef8250f75c26cc5f1d".into(),
        200,
        headers,
        "{}".into(),
    );
    // Single frame: final, non-streaming, first chunk.
    assert!(!f.is_streaming);
    assert_eq!(f.chunk_index, 0);
    assert!(f.is_final);
    // Success shape: head fields present, no error.
    assert_eq!(f.status_code, Some(200));
    assert_eq!(
        f.headers
            .as_ref()
            .and_then(|h| h.get("Content-Type"))
            .map(String::as_str),
        Some("application/json")
    );
    assert_eq!(f.body, "{}");
    assert!(f.error.is_none());
    assert!(f.check_head_fields().is_ok());
}

#[test]
fn error_omits_head_fields_and_carries_typed_error() {
    let f = ResponseFrame::error(
        "019e2d7a444e70ef8250f75c26cc5f1d".into(),
        FrameError {
            code: ErrorCode::AllowlistDeny,
            message: "denied".into(),
            retryable: false,
        },
    );
    // Same single-chunk invariant as reply.
    assert!(!f.is_streaming);
    assert_eq!(f.chunk_index, 0);
    assert!(f.is_final);
    // Error shape: no status or headers, empty body, typed error set.
    assert_eq!(f.status_code, None);
    assert!(f.headers.is_none());
    assert!(f.body.is_empty());
    assert!(matches!(
        f.error.as_ref().map(|e| &e.code),
        Some(ErrorCode::AllowlistDeny)
    ));
    assert!(f.check_head_fields().is_ok());
}

#[test]
fn request_id_validator() {
    assert!(is_valid_request_id("019e2d7a444e70ef8250f75c26cc5f1d"));
    assert!(!is_valid_request_id("not-hex"));
    assert!(!is_valid_request_id("ABCDEF"));
    assert!(!is_valid_request_id(""));
    assert!(!is_valid_request_id(
        "019e2d7a444e70ef8250f75c26cc5f1d019e2d7a"
    ));
}

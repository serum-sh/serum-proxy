//! WebSocket envelope between serum and the customer-deployed proxy.
//!
//! The source of truth is the shared protocol vectors in `docs/protocol-vectors.json`
//! (verified by `tests/protocol_vectors.rs`); the wire spec is `docs/protocol.md`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u8 = 1;

/// Closed set of typed error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    #[serde(rename = "403_ALLOWLIST")]
    AllowlistDeny,
    #[serde(rename = "404_UNKNOWN_SERVICE")]
    UnknownService,
    #[serde(rename = "405_METHOD")]
    MethodNotAllowed,
    #[serde(rename = "413_BODY_LIMIT")]
    BodyLimitExceeded,
    #[serde(rename = "429_CONCURRENT_REQUESTS")]
    ConcurrencyExceeded,
    #[serde(rename = "502_BACKEND_UNREACHABLE")]
    BackendUnreachable,
    #[serde(rename = "504_TIMEOUT")]
    UpstreamTimeout,
    #[serde(rename = "PROXY_DISCONNECTED")]
    ProxyDisconnected,
    #[serde(rename = "VERSION_MISMATCH")]
    VersionMismatch,
    #[serde(rename = "FRAME_PARSE_FAILED")]
    FrameParseFailed,
}

/// Typed error reported inside a `ResponseFrame` or `ErrorFrameBody`.
///
/// When set on a response, the response is a transport failure rather than
/// a backend reply; `status_code` and `body` are not meaningful. `retryable`
/// is advisory: 429 may succeed on retry, 403 will not.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FrameError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

/// What the proxy stripped from a response body before forwarding.
///
/// Feeds the local egress panel and the serum-side aggregated view.
/// `None` on `ResponseFrame` means no redaction occurred.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RedactionSummary {
    pub rules_fired: Vec<String>,
    pub bytes_redacted: u64,
}

/// One query-string entry; either a single value or a repeated key.
///
/// Serialized into the URL via `urlencode(..., doseq=True)` semantics: a list
/// becomes one `name=value` pair per item (`docs/protocol.md`, "Query serialization").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum QueryValue {
    One(String),
    Many(Vec<String>),
}

/// The closed set of methods the proxy accepts: `GET` and `POST` only.
///
/// Any other method (PUT, DELETE, PATCH, etc.) fails at envelope deserialize.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
}

impl HttpMethod {
    /// The uppercase wire token (`GET` / `POST`) for logging and error messages.
    pub fn as_str(&self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
        }
    }
}

/// `protocol_version: 1` constant on every post-handshake frame.
///
/// Serde emits the literal `1` and rejects anything else. A zero-sized type with a custom
/// (de)serializer, not a plain `u8`, makes an out-of-range value fail at the deserialize
/// boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProtocolV1;

impl Serialize for ProtocolV1 {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(PROTOCOL_VERSION)
    }
}

impl<'de> Deserialize<'de> for ProtocolV1 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = u8::deserialize(d)?;
        if v == PROTOCOL_VERSION {
            Ok(ProtocolV1)
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported protocol_version {v}; this proxy speaks {PROTOCOL_VERSION}",
            )))
        }
    }
}

/// Version-handshake frame sent both directions on WS open.
///
/// Proxy to serum: populates `protocol_versions_supported` and `proxy_build_version`.
/// serum to proxy: populates `protocol_version` to ack the chosen version.
/// On no overlap, serum closes the WS with code 4001 instead of acking.
///
/// The one frame without `deny_unknown_fields`: hello negotiates the version, so it cannot
/// assume the sender's field set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HelloFrame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_versions_supported: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_build_version: Option<String>,
}

/// Periodic liveness ping from proxy to serum.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatFrame {
    #[serde(default)]
    pub protocol_version: ProtocolV1,
    pub uptime_seconds: u64,
    pub requests_handled_since_last: u64,
}

/// serum asks the proxy to make one HTTP call against a local backend.
///
/// `target_service` is the kind key (e.g. `prometheus`); the proxy maps it to the URL and auth of
/// the service with that `type` in local config. `request_id` is UUIDv7 hex.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RequestFrame {
    #[serde(default)]
    pub protocol_version: ProtocolV1,
    pub request_id: String,
    pub target_service: String,
    pub method: HttpMethod,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<BTreeMap<String, QueryValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// Proxy's reply to one `RequestFrame` (matched on `request_id`).
///
/// By design, a response carries either a backend reply OR a proxy-level error, never both:
///
/// - Backend reply: `error` is `None`; `status_code` and `body` describe what the upstream
///   returned (including 4xx/5xx from the upstream - those are still valid backend responses).
/// - Proxy-level error: `error` is set (typed code + message); `status_code`
///   and `body` are not meaningful and consumers should ignore them.
///
/// The proxy always sends one frame with `is_streaming=false, is_final=true`. The streaming fields
/// are for a later version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResponseFrame {
    #[serde(default)]
    pub protocol_version: ProtocolV1,
    pub request_id: String,
    #[serde(default)]
    pub is_streaming: bool,
    #[serde(default)]
    pub chunk_index: u32,
    #[serde(default = "default_true")]
    pub is_final: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<FrameError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redaction_summary: Option<RedactionSummary>,
}

fn default_true() -> bool {
    true
}

/// Out-of-band protocol error not tied to a specific request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ErrorFrameBody {
    #[serde(default)]
    pub protocol_version: ProtocolV1,
    pub error: FrameError,
}

/// Tagged union over all frame types, discriminated by `type`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Hello(HelloFrame),
    Heartbeat(HeartbeatFrame),
    Request(RequestFrame),
    Response(ResponseFrame),
    Error(ErrorFrameBody),
}

/// `^[0-9a-f]{32}$` (UUIDv7 hex); see `docs/protocol.md`, "Validation layers".
pub fn is_valid_request_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl ResponseFrame {
    /// Single-chunk success response: status, headers and body in one final frame.
    ///
    /// Always a single frame (`is_streaming=false, chunk_index=0, is_final=true`), so callers
    /// never set these fields. See `check_head_fields` for the read-side rule.
    pub fn reply(
        request_id: String,
        status: u16,
        headers: BTreeMap<String, String>,
        body: String,
    ) -> Self {
        Self {
            protocol_version: ProtocolV1,
            request_id,
            is_streaming: false,
            chunk_index: 0,
            is_final: true,
            status_code: Some(status),
            headers: Some(headers),
            body,
            error: None,
            redaction_summary: None,
        }
    }

    /// Single-chunk error response: no status or headers, just the typed error.
    ///
    /// Also a single frame, like `reply`. Used for every rejection that still owes the peer a
    /// per-request response frame.
    pub fn error(request_id: String, error: FrameError) -> Self {
        Self {
            protocol_version: ProtocolV1,
            request_id,
            is_streaming: false,
            chunk_index: 0,
            is_final: true,
            status_code: None,
            headers: None,
            body: String::new(),
            error: Some(error),
            redaction_summary: None,
        }
    }

    /// `status_code` and headers are set on the first frame only; subsequent
    /// chunks omit them (the head is already on the wire). Wire spec:
    /// `docs/protocol.md`, "Validation layers".
    pub fn check_head_fields(&self) -> Result<(), &'static str> {
        if self.chunk_index > 0 && (self.status_code.is_some() || self.headers.is_some()) {
            return Err("status_code and headers must be omitted on chunk_index > 0");
        }
        Ok(())
    }
}

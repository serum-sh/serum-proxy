//! `RequestOutcome` projections: one outcome maps to a wire response and an egress
//! row that cannot drift. Tested by value, with no backend or executor.

use std::collections::BTreeMap;

use serum_proxy::{
    egress::strip_query,
    envelope::{ErrorCode, HttpMethod, ProtocolV1, RequestFrame},
    error::ProxyError,
    executor::RequestOutcome,
};

fn req(target: &str, method: HttpMethod, path: &str) -> RequestFrame {
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
fn response_outcome_projects_to_both_records() {
    let mut headers = BTreeMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    let r = req("prometheus", HttpMethod::Get, "/api/v1/query?q=up");
    let outcome = RequestOutcome::Response {
        status: 200,
        headers,
        body: "{}".into(),
        bytes: 2,
    };

    // Egress borrows; identity fields come from the request, query stripped.
    let egress = outcome.to_egress(&r);
    assert_eq!(egress.status, Some(200));
    assert_eq!(egress.response_bytes, Some(2));
    assert_eq!(egress.error_code, None);
    assert_eq!(egress.request_id, r.request_id);
    assert_eq!(egress.target_service, "prometheus");
    assert_eq!(egress.method, HttpMethod::Get.as_str());
    assert_eq!(egress.path, strip_query("/api/v1/query?q=up"));

    // Response consumes the outcome (moves the body out).
    let resp = outcome.to_response(r.request_id.clone());
    assert_eq!(resp.status_code, Some(200));
    assert_eq!(resp.body, "{}");
    assert!(resp.error.is_none());

    // No drift: both records report the same status.
    assert_eq!(resp.status_code, egress.status);
}

#[test]
fn failed_outcome_projects_same_code_to_both_records() {
    let r = req("es", HttpMethod::Post, "/_bulk");
    let outcome = RequestOutcome::Failed(ProxyError::AllowlistDeny {
        service: "es".into(),
        path: "/_bulk".into(),
    });

    let egress = outcome.to_egress(&r);
    assert_eq!(egress.status, None);
    assert_eq!(egress.response_bytes, None);

    let resp = outcome.to_response(r.request_id.clone());
    assert_eq!(resp.status_code, None);
    assert!(resp.headers.is_none());
    assert!(resp.body.is_empty());
    let fe = resp.error.expect("error frame");
    assert!(matches!(fe.code, ErrorCode::AllowlistDeny));

    // No drift: the egress error_code is the Debug of the same code the response
    // carries, both derived from the one ProxyError.
    assert_eq!(egress.error_code, Some(format!("{:?}", fe.code)));
}

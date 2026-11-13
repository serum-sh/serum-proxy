//! A rejected request logs a line with its request ID, so an operator can find the line in the
//! log store with the ID that serum records.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use serum_proxy::{
    config::{ServiceConfig, ServiceKind},
    egress::EgressBuffer,
    envelope::{ErrorCode, Frame, HttpMethod, ProtocolV1, RequestFrame},
    executor::{Executor, ResolvedService, build_client},
    logging::json_layer,
    session::{Action, ServiceRegistry, decide},
};
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::prelude::*;

const REQUEST_ID: &str = "019e2d7a444e70ef8250f75c26cc5f1d";

/// Collects the JSON log lines of one test.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Logs {
    /// Send the logs of this thread through the proxy JSON layer into `self`, until the guard
    /// drops.
    fn capture(&self) -> DefaultGuard {
        let logs = self.clone();
        let subscriber = tracing_subscriber::registry().with(json_layer(move || logs.clone()));
        tracing::subscriber::set_default(subscriber)
    }

    /// The first line whose message starts with `prefix`.
    fn line(&self, prefix: &str) -> Value {
        let buf = self.0.lock().unwrap();
        std::str::from_utf8(&buf)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|v| {
                v["fields"]["message"]
                    .as_str()
                    .is_some_and(|m| m.starts_with(prefix))
            })
            .unwrap_or_else(|| panic!("no log line starts with {prefix:?}"))
    }
}

fn request(target_service: &str) -> RequestFrame {
    RequestFrame {
        protocol_version: ProtocolV1,
        request_id: REQUEST_ID.into(),
        target_service: target_service.into(),
        method: HttpMethod::Post,
        path: "/logs-*/_search".into(),
        query: None,
        headers: None,
        body: None,
    }
}

#[tokio::test]
async fn executor_rejection_logs_the_request_span() {
    // Refused port: the request passes the guardrails and fails with 502, as a call to an
    // Elasticsearch with zero replicas does.
    let cfg = ServiceConfig {
        kind: ServiceKind::Elasticsearch,
        base_url: "http://127.0.0.1:1".into(),
        headers: BTreeMap::new(),
        max_concurrent_requests: 4,
        path_allowlist: None,
        tls_ca_cert: None,
        auth_token_file: None,
    };
    let service = ResolvedService::from_config(&cfg).unwrap();
    let exec = Executor::new(build_client(), EgressBuffer::new(64));

    let logs = Logs::default();
    let _guard = logs.capture();
    let resp = exec.execute(request("elasticsearch"), &service).await;
    assert_eq!(resp.error.unwrap().code, ErrorCode::BackendUnreachable);

    let line = logs.line("request rejected");
    assert_eq!(line["fields"]["error_code"], "BackendUnreachable");
    let span = &line["span"];
    assert_eq!(span["name"], "execute");
    assert_eq!(span["request_id"], REQUEST_ID);
    assert_eq!(span["target_service"], "elasticsearch");
    assert_eq!(span["method"], "POST");
    assert_eq!(span["path"], "/logs-*/_search");
}

#[test]
fn unknown_service_logs_request_id_and_target_service() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let action = decide(Ok(Frame::Request(request("nope"))), &ServiceRegistry::new());
    assert!(matches!(action, Action::Reject(_)));

    let line = logs.line("unknown target_service");
    assert_eq!(line["level"], "WARN");
    assert_eq!(line["fields"]["request_id"], REQUEST_ID);
    assert_eq!(line["fields"]["target_service"], "nope");
}

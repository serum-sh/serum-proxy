//! `ResolvedService::from_config` startup-failure modes: a bad CA bundle or
//! token file must fail at assembly with an error naming the service and not
//! later as a broken client. Cases: unreadable CA, certless CA, empty token file.

use std::collections::BTreeMap;

use serum_proxy::{
    config::{ServiceConfig, ServiceKind},
    executor::ResolvedService,
};

/// A minimal service built by value so a plain-http `base_url` skips config's
/// https validation; the caller overrides the field under test.
fn svc() -> ServiceConfig {
    ServiceConfig {
        kind: ServiceKind::Kubernetes,
        base_url: "http://127.0.0.1:1".into(),
        headers: BTreeMap::new(),
        max_concurrent_requests: 4,
        path_allowlist: None,
        tls_ca_cert: None,
        auth_token_file: None,
    }
}

/// Expect `from_config` to fail and return its error message. `ResolvedService`
/// holds credentials and isn't Debug, so match instead of `unwrap_err`.
fn from_config_err(s: &ServiceConfig) -> String {
    match ResolvedService::from_config(s) {
        Ok(_) => panic!("from_config must reject this service"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn from_config_surfaces_unreadable_ca_bundle() {
    let mut s = svc();
    s.tls_ca_cert = Some("/no/such/ca.pem".into());
    let msg = from_config_err(&s);
    assert!(
        msg.contains("service kubernetes"),
        "names the service: {msg}"
    );
    assert!(msg.contains("/no/such/ca.pem"), "names the path: {msg}");
}

#[test]
fn from_config_surfaces_certless_ca_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let ca_path = dir.path().join("empty-ca.pem");
    std::fs::write(&ca_path, "not a certificate\n").unwrap();

    let mut s = svc();
    s.tls_ca_cert = Some(ca_path.display().to_string());
    let msg = from_config_err(&s);
    assert!(
        msg.contains("service kubernetes"),
        "names the service: {msg}"
    );
    assert!(msg.contains("no certificates"), "explains the cause: {msg}");
}

#[test]
fn from_config_surfaces_empty_token_file() {
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, "   \n").unwrap();

    let mut s = svc();
    s.auth_token_file = Some(token_path.display().to_string());
    let msg = from_config_err(&s);
    assert!(
        msg.contains("service kubernetes"),
        "names the service: {msg}"
    );
    assert!(msg.contains("empty"), "explains the cause: {msg}");
}

#[test]
fn from_config_populates_all_fields_on_good_input() {
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, "token-v1\n").unwrap();

    let mut s = svc();
    s.auth_token_file = Some(token_path.display().to_string());
    let resolved = ResolvedService::from_config(&s).expect("service resolves");

    // Config cloned in; guardrails compiled; token source built; no CA -> no TLS client.
    assert_eq!(resolved.config.kind, ServiceKind::Kubernetes);
    assert_eq!(resolved.guardrails.service_name, "kubernetes");
    assert!(
        resolved.tls_client.is_none(),
        "no tls_ca_cert -> shared client"
    );
    assert!(
        resolved.token_source.is_some(),
        "auth_token_file -> token source"
    );
}

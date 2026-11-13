//! Per-service TLS CA: a service with `tls_ca_cert` gets a per-service client
//! trusting that CA; everything else stays on the shared public-roots client.

use std::collections::HashMap;

use serum_proxy::{
    config::{Config, ServiceKind},
    executor::{ResolvedService, build_client, build_service_client},
};

// A self-signed CA so `build_service_client` exercises the real PEM -> rustls path.
const TEST_CA_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIIDETCCAfmgAwIBAgIUFsQOblsHE9TicdRMPAEa7gkrno0wDQYJKoZIhvcNAQEL
BQAwGDEWMBQGA1UEAwwNc2VydW0gdGVzdCBDQTAeFw0yNjExMTMxOTMyNTRaFw0z
NjExMTAxOTMyNTRaMBgxFjAUBgNVBAMMDXNlcnVtIHRlc3QgQ0EwggEiMA0GCSqG
SIb3DQEBAQUAA4IBDwAwggEKAoIBAQDIbZkyykWyvLNU2xZ1mZaJ/rlQ/vJzKuQ5
ZSHCOkYIy0/OvxiZrNXNKZay1tTfUIIFngikcHPzpmK482qAy/Hez0jpb/pBBnK2
c/fMfG0QeCirw4Fu2D8717yfniq4Qa/VPTl83mLux4WFppWR9Xg4oPY8G9eLJWiL
RIasICfLPMwOiiFnLiiPTb5FDs207pkO6mi2bjiaviz954j/MffkPjAlJdIMpJfh
ZXSLpd7TH1yNPO7uARSoDgkPkDCmv/GgQMAV8F/KCPYhn4jjdiOc2Ae9sJuQdjVE
qXQYsZjSP9pcnuVKD97quSa9q3DGDDl2q2E0eWk4o2brEQAlRWbDAgMBAAGjUzBR
MB0GA1UdDgQWBBQFBFCQlIeiDPzDti63kh73nFGIszAfBgNVHSMEGDAWgBQFBFCQ
lIeiDPzDti63kh73nFGIszAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUA
A4IBAQB1SftR0gEsxpYd3DR/rbd2RTSlw1bibroMkOSoeAG3aQPW+cBZmK6j6zl7
EaYe4+LbhCL1y5BvpIRCTO6CcizIxY91fhNk2p5U/xbh7gSDErj0x7DCNdn8fEVF
pSET3Y90CNM8TwjIece/7JavPew0le/eiZSfbNrnwSUxO4CJHgmbYAY4H/GlTgHL
lf4GiGrgFCpxBiFId9oHvWgWN+KpPkmX02faqxRRHOXFgph1dh6ZKGtGrWSOgrn9
BwqytntWLKZbqs/g8Gof9/hUsTgs/80/FAPMCzriUD2bldLA1AM+rbJIQ6MsKGPM
uwBoC+t/oeZ2QCjDOcwbCfgCglCY
-----END CERTIFICATE-----
";

/// Resolve a service the way `main.rs` builds the registry, then return whether
/// `client()` picked the per-service client over the shared one.
fn uses_per_service_client(cfg: &Config, kind: ServiceKind, shared: &reqwest::Client) -> bool {
    let svc = cfg
        .services
        .iter()
        .find(|s| s.kind == kind)
        .expect("service present in config");
    let resolved = ResolvedService::from_config(svc).expect("service resolves");
    // `client()` returns `shared` itself (same address) when no CA is configured,
    // and the borrowed per-service client otherwise.
    !std::ptr::eq(resolved.client(shared), shared)
}

#[test]
fn per_service_client_selected_only_when_ca_configured() {
    let dir = tempfile::tempdir().unwrap();
    let ca_path = dir.path().join("k8s-ca.pem");
    std::fs::write(&ca_path, TEST_CA_PEM).unwrap();

    let yaml = format!(
        r#"
proxy:
  serum_wss_url: "wss://gateway.example.com/v1/proxy"
services:
  - type: "prometheus"
    base_url: "http://prom:9090"
  - type: "elasticsearch"
    base_url: "http://es:9200"
  - type: "kubernetes"
    base_url: "https://kubernetes.default.svc"
    tls_ca_cert: "{}"
"#,
        ca_path.display()
    );

    let env: HashMap<&str, &str> = HashMap::new();
    let cfg =
        Config::from_yaml_str(&yaml, |k| env.get(k).map(|s| s.to_string())).expect("config loads");
    let shared = build_client();

    // Kubernetes set a CA -> per-service client; the others trust public roots.
    assert!(
        uses_per_service_client(&cfg, ServiceKind::Kubernetes, &shared),
        "kubernetes with tls_ca_cert must use the per-service client"
    );
    assert!(
        !uses_per_service_client(&cfg, ServiceKind::Prometheus, &shared),
        "prometheus must stay on the shared client"
    );
    assert!(
        !uses_per_service_client(&cfg, ServiceKind::Elasticsearch, &shared),
        "elasticsearch must stay on the shared client"
    );
}

#[test]
fn missing_ca_file_is_a_named_error() {
    // A configured-but-unreadable CA path must fail loudly at startup, not silently
    // fall back to the shared client.
    let yaml = r#"
proxy:
  serum_wss_url: "wss://gateway.example.com/v1/proxy"
services:
  - type: "kubernetes"
    base_url: "https://kubernetes.default.svc"
    tls_ca_cert: "/no/such/ca.pem"
"#;
    let env: HashMap<&str, &str> = HashMap::new();
    let cfg =
        Config::from_yaml_str(yaml, |k| env.get(k).map(|s| s.to_string())).expect("config loads");
    let err = build_service_client(&cfg.services[0]).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("service kubernetes"),
        "error names the service: {msg}"
    );
    assert!(
        msg.contains("/no/such/ca.pem"),
        "error names the path: {msg}"
    );
}

#[test]
fn certless_ca_file_is_a_named_error() {
    // A readable CA file holding no certificate must fail loudly, not build a client
    // that silently trusts only public roots.
    let dir = tempfile::tempdir().unwrap();
    let ca_path = dir.path().join("empty-ca.pem");
    std::fs::write(&ca_path, "not a certificate\n").unwrap();

    let yaml = format!(
        r#"
proxy:
  serum_wss_url: "wss://gateway.example.com/v1/proxy"
services:
  - type: "kubernetes"
    base_url: "https://kubernetes.default.svc"
    tls_ca_cert: "{}"
"#,
        ca_path.display()
    );
    let env: HashMap<&str, &str> = HashMap::new();
    let cfg =
        Config::from_yaml_str(&yaml, |k| env.get(k).map(|s| s.to_string())).expect("config loads");
    let err = build_service_client(&cfg.services[0]).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("service kubernetes"),
        "error names the service: {msg}"
    );
    assert!(
        msg.contains("no certificates"),
        "error explains the cause: {msg}"
    );
}

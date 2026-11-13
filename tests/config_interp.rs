//! YAML config loading with `${ENV}` interpolation.

use std::collections::HashMap;

use serum_proxy::config::{Config, ConfigError, ServiceKind};

fn lookup<'a>(env: &'a HashMap<&'a str, &'a str>) -> impl Fn(&str) -> Option<String> + 'a {
    move |k: &str| env.get(k).map(|s| s.to_string())
}

const BASE_YAML: &str = r#"
proxy:
  serum_wss_url: "wss://gateway.example.com/v1/proxy"
  log_level: "INFO"

services:
  - type: "prometheus"
    base_url: "http://prom:9090"
  - type: "elasticsearch"
    base_url: "http://es:9200"
    headers:
      Authorization: "Basic ${ES_TOKEN}"
"#;

#[test]
fn loads_with_env_var_present() {
    let env: HashMap<&str, &str> = [("ES_TOKEN", "secret-abc")].into_iter().collect();
    let cfg = Config::from_yaml_str(BASE_YAML, lookup(&env)).expect("config loads");
    assert_eq!(
        cfg.proxy.serum_wss_url,
        "wss://gateway.example.com/v1/proxy"
    );
    assert_eq!(cfg.services.len(), 2);
    assert_eq!(cfg.services[0].kind, ServiceKind::Prometheus);
    assert_eq!(cfg.services[1].kind, ServiceKind::Elasticsearch);
    assert_eq!(
        cfg.services[1]
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some("Basic secret-abc"),
    );
}

#[test]
fn missing_env_var_errors_named() {
    let env: HashMap<&str, &str> = HashMap::new();
    let err = Config::from_yaml_str(BASE_YAML, lookup(&env)).unwrap_err();
    match err {
        ConfigError::MissingEnv(k) => assert_eq!(k, "ES_TOKEN"),
        other => panic!("expected MissingEnv, got {other:?}"),
    }
}

#[test]
fn admin_bind_takes_config_then_env_then_loopback() {
    const UNSET: &str = r#"
proxy:
  serum_wss_url: "wss://gateway.example.com/v1/proxy"
services: []
"#;
    const SET: &str = r#"
proxy:
  serum_wss_url: "wss://gateway.example.com/v1/proxy"
  admin_bind: "127.0.0.1:9000"
services: []
"#;
    let none: HashMap<&str, &str> = HashMap::new();
    let container: HashMap<&str, &str> =
        [("SERUM_ADMIN_BIND", "0.0.0.0:8088")].into_iter().collect();
    let bind = |yaml, env: &HashMap<&str, &str>| {
        Config::from_yaml_str(yaml, lookup(env))
            .expect("config loads")
            .proxy
            .admin_bind
    };

    assert_eq!(bind(UNSET, &none), "127.0.0.1:8088");
    assert_eq!(bind(UNSET, &container), "0.0.0.0:8088");
    // An operator who narrowed the bind keeps it, container or not.
    assert_eq!(bind(SET, &container), "127.0.0.1:9000");
}

#[test]
fn rejects_non_wss_url() {
    let yaml = r#"
proxy:
  serum_wss_url: "https://gateway.example.com/v1/proxy"
services: []
"#;
    let env: HashMap<&str, &str> = HashMap::new();
    let err = Config::from_yaml_str(yaml, lookup(&env)).unwrap_err();
    assert!(matches!(err, ConfigError::Invalid(_)));
}

#[test]
fn rejects_ws_url_without_opt_in() {
    let yaml = r#"
proxy:
  serum_wss_url: "ws://serum:8010/v1/proxy"
services: []
"#;
    let env: HashMap<&str, &str> = HashMap::new();
    let err = Config::from_yaml_str(yaml, lookup(&env)).unwrap_err();
    assert!(matches!(err, ConfigError::Invalid(_)));
}

#[test]
fn allows_ws_url_with_opt_in() {
    let yaml = r#"
proxy:
  serum_wss_url: "ws://serum:8010/v1/proxy"
  allow_insecure_ws: true
services: []
"#;
    let env: HashMap<&str, &str> = HashMap::new();
    let cfg = Config::from_yaml_str(yaml, lookup(&env)).expect("ws:// loads when opted in");
    assert_eq!(cfg.proxy.serum_wss_url, "ws://serum:8010/v1/proxy");
    assert!(cfg.proxy.allow_insecure_ws);
}

#[test]
fn rejects_two_services_of_one_type() {
    let yaml = r#"
proxy:
  serum_wss_url: "wss://gateway.example.com/v1/proxy"
services:
  - type: "prometheus"
    base_url: "http://a:9090"
  - type: "prometheus"
    base_url: "http://b:9090"
"#;
    let env: HashMap<&str, &str> = HashMap::new();
    let err = Config::from_yaml_str(yaml, lookup(&env)).unwrap_err();
    assert!(matches!(err, ConfigError::Invalid(_)));
}

#[test]
fn rejects_tls_ca_cert_on_non_https_base_url() {
    let yaml = r#"
proxy:
  serum_wss_url: "wss://gateway.example.com/v1/proxy"
services:
  - type: "prometheus"
    base_url: "http://prom:9090"
    tls_ca_cert: "/etc/ca.pem"
"#;
    let env: HashMap<&str, &str> = HashMap::new();
    let err = Config::from_yaml_str(yaml, lookup(&env)).unwrap_err();
    assert!(matches!(err, ConfigError::Invalid(_)));
}

#[test]
fn rejects_auth_token_file_on_non_https_base_url() {
    let yaml = r#"
proxy:
  serum_wss_url: "wss://gateway.example.com/v1/proxy"
services:
  - type: "prometheus"
    base_url: "http://prom:9090"
    auth_token_file: "/var/run/secrets/token"
"#;
    let env: HashMap<&str, &str> = HashMap::new();
    let err = Config::from_yaml_str(yaml, lookup(&env)).unwrap_err();
    assert!(matches!(err, ConfigError::Invalid(_)));
}

#[test]
fn allows_https_base_url_without_tls_ca_cert() {
    // A public-CA https backend needs no tls_ca_cert; it defaults to public roots.
    let yaml = r#"
proxy:
  serum_wss_url: "wss://gateway.example.com/v1/proxy"
services:
  - type: "prometheus"
    base_url: "https://prom.example.com"
"#;
    let env: HashMap<&str, &str> = HashMap::new();
    let cfg = Config::from_yaml_str(yaml, lookup(&env)).expect("https without CA loads");
    assert!(cfg.services[0].tls_ca_cert.is_none());
}

#[test]
fn rejects_unknown_top_level_key() {
    let yaml = r#"
proxy:
  serum_wss_url: "wss://gateway.example.com/v1/proxy"
services: []
extra: "not allowed"
"#;
    let env: HashMap<&str, &str> = HashMap::new();
    let err = Config::from_yaml_str(yaml, lookup(&env)).unwrap_err();
    assert!(matches!(err, ConfigError::Parse(_)));
}

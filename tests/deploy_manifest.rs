//! Deploy manifest templates: the marker contract, and the shape of what they
//! render to. The renderer lives outside this repo, so these tests stand in for
//! it: they substitute the markers the same way and assert on the result.

use std::collections::BTreeSet;
use std::net::SocketAddr;

use regex::Regex;
use serde::Deserialize;
use serde_yaml::Value;
use serum_proxy::config::Config;

const CORE: &str = include_str!("../deploy/kubernetes/core.yaml");
const RBAC: &str = include_str!("../deploy/kubernetes/rbac.yaml");

const NAMESPACE: &str = "serum";
const IMAGE: &str = "registry.example.com/serum-proxy:1.4.0";

/// A config the proxy itself accepts, so the ConfigMap round-trip is a real check.
const SAMPLE_CONFIG: &str = r#"proxy:
  serum_wss_url: "wss://gateway.example.com/v1/proxy"
  admin_bind: "0.0.0.0:8088"

services:
  - type: "elasticsearch"
    base_url: "http://elasticsearch.observability.svc:9200"
    headers:
      Authorization: "Basic ${ES_TOKEN}"
"#;

/// The least a renderer can emit: every optional setting left to its default.
const MINIMAL_CONFIG: &str = r#"proxy:
  serum_wss_url: "wss://gateway.example.com/v1/proxy"

services: []
"#;

fn marker_re() -> Regex {
    Regex::new(r"%\{([a-z_]+)\}").expect("static regex")
}

fn markers(template: &str) -> BTreeSet<String> {
    marker_re()
        .captures_iter(template)
        .map(|c| c[1].to_string())
        .collect()
}

/// Indent every line after the first, the rule a block-scalar marker imposes.
fn indent_continuation(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    let mut out = String::new();
    for (i, line) in text.trim_end().lines().enumerate() {
        if i > 0 {
            out.push('\n');
            if !line.is_empty() {
                out.push_str(&pad);
            }
        }
        out.push_str(line);
    }
    out
}

fn render(template: &str, values: &[(&str, String)]) -> String {
    let mut out = template.to_string();
    for (name, value) in values {
        out = out.replace(&format!("%{{{name}}}"), value);
    }
    assert!(
        !marker_re().is_match(&out),
        "unsubstituted marker left in rendered output"
    );
    out
}

fn render_core(service_account: &str) -> Vec<Value> {
    render_core_in(NAMESPACE, service_account, SAMPLE_CONFIG)
}

fn render_core_with(service_account: &str, config: &str) -> Vec<Value> {
    render_core_in(NAMESPACE, service_account, config)
}

fn render_core_in(namespace: &str, service_account: &str, config: &str) -> Vec<Value> {
    let rendered = render(
        CORE,
        &[
            ("namespace", namespace.to_string()),
            ("image", IMAGE.to_string()),
            ("service_account", service_account.to_string()),
            ("config_yaml", indent_continuation(config, 4)),
        ],
    );
    docs(&rendered)
}

fn render_rbac() -> Vec<Value> {
    render_rbac_in(NAMESPACE)
}

fn render_rbac_in(namespace: &str) -> Vec<Value> {
    let rendered = render(RBAC, &[("namespace", namespace.to_string())]);
    docs(&rendered)
}

/// The config the pod actually loads, as the proxy parses it, with the pod's
/// environment: a backend credential from the Secret, and the image's own vars.
fn mounted_config(docs: &[Value]) -> Config {
    let mounted = by_kind(docs, "ConfigMap")["data"]["config.yaml"]
        .as_str()
        .expect("config.yaml key holds a string");
    Config::from_yaml_str(mounted, |k| match k {
        "ES_TOKEN" => Some("secret".to_string()),
        "SERUM_ADMIN_BIND" => Some(image_admin_bind()),
        _ => None,
    })
    .expect("the mounted config loads")
}

/// `SERUM_ADMIN_BIND` from the runtime stage, the last stage of the Dockerfile.
/// What a config that omits `admin_bind` binds to in the pod.
fn image_admin_bind() -> String {
    include_str!("../Dockerfile")
        .lines()
        .filter_map(|l| l.trim().strip_prefix("ENV SERUM_ADMIN_BIND="))
        .next_back()
        .expect("the image sets SERUM_ADMIN_BIND")
        .to_string()
}

fn docs(rendered: &str) -> Vec<Value> {
    serde_yaml::Deserializer::from_str(rendered)
        .map(|d| Value::deserialize(d).expect("rendered manifest is valid YAML"))
        .collect()
}

fn kinds(docs: &[Value]) -> Vec<&str> {
    docs.iter()
        .map(|d| d["kind"].as_str().expect("every doc has a kind"))
        .collect()
}

fn by_kind<'a>(docs: &'a [Value], kind: &str) -> &'a Value {
    docs.iter()
        .find(|d| d["kind"].as_str() == Some(kind))
        .unwrap_or_else(|| panic!("no {kind} in this file"))
}

fn container(docs: &[Value]) -> &Value {
    let containers = by_kind(docs, "Deployment")["spec"]["template"]["spec"]["containers"]
        .as_sequence()
        .expect("containers is a sequence");
    assert_eq!(containers.len(), 1, "one container in the pod");
    &containers[0]
}

#[test]
fn markers_are_the_documented_set() {
    assert_eq!(
        markers(CORE),
        ["config_yaml", "image", "namespace", "service_account"]
            .map(String::from)
            .into()
    );
    assert_eq!(markers(RBAC), ["namespace"].map(String::from).into());
}

#[test]
fn namespaced_resources_take_the_rendered_namespace() {
    // Not the default namespace, so a hardcoded one cannot pass.
    const NS: &str = "serum-staging";
    let core = render_core_in(NS, "serum-proxy", SAMPLE_CONFIG);

    // kubectl applies documents in order, so the namespace has to come first.
    assert_eq!(kinds(&core)[0], "Namespace");
    assert_eq!(by_kind(&core, "Namespace")["metadata"]["name"], NS);

    for doc in [core, render_rbac_in(NS)].concat() {
        if let Some(ns) = doc["metadata"]["namespace"].as_str() {
            assert_eq!(ns, NS);
        }
    }
}

#[test]
fn config_map_carries_a_config_the_proxy_accepts() {
    let docs = render_core("serum-proxy");
    let mounted = by_kind(&docs, "ConfigMap")["data"]["config.yaml"]
        .as_str()
        .expect("config.yaml key holds a string");

    assert_eq!(mounted.trim_end(), SAMPLE_CONFIG.trim_end());
    assert_eq!(mounted_config(&docs).services.len(), 1);
}

/// A config with no `admin_bind` falls back to the image's env, not the loopback
/// default, or the kubelet could not reach /healthz and the pod would crash-loop.
#[test]
fn the_probe_reaches_the_admin_port_on_a_config_that_sets_nothing() {
    for docs in [
        render_core_with("serum-proxy", MINIMAL_CONFIG),
        render_core("serum-proxy"),
    ] {
        let bind: SocketAddr = mounted_config(&docs)
            .proxy
            .admin_bind
            .parse()
            .expect("admin_bind is an address");
        assert!(!bind.ip().is_loopback(), "admin_bind is loopback: {bind}");

        let container = container(&docs);
        let port = Some(u64::from(bind.port()));
        assert_eq!(container["ports"][0]["containerPort"].as_u64(), port);
        assert_eq!(container["livenessProbe"]["httpGet"]["port"].as_u64(), port);
    }
}

#[test]
fn config_map_is_mounted_where_the_binary_looks_for_it() {
    let docs = render_core("serum-proxy");
    let container = container(&docs);
    let mount = &container["volumeMounts"][0];

    assert_eq!(mount["mountPath"], "/app/config.yaml");
    assert_eq!(mount["subPath"], "config.yaml");
    assert_eq!(mount["readOnly"], true);
    assert_eq!(
        by_kind(&docs, "Deployment")["spec"]["template"]["spec"]["volumes"][0]["configMap"]["name"],
        by_kind(&docs, "ConfigMap")["metadata"]["name"],
    );
}

#[test]
fn token_reaches_the_pod_through_the_secret_reference() {
    let docs = render_core("serum-proxy");
    let env = &container(&docs)["env"][0];

    assert_eq!(env["name"], "SERUM_PROXY_TOKEN");
    assert_eq!(env["valueFrom"]["secretKeyRef"]["name"], "serum-proxy");
    assert_eq!(env["valueFrom"]["secretKeyRef"]["key"], "token");
    // Backend credentials written as ${VAR} in the config come from the rest
    // of the same Secret.
    assert_eq!(
        container(&docs)["envFrom"][0]["secretRef"]["name"],
        "serum-proxy"
    );
}

#[test]
fn templates_hold_no_secret_material() {
    for template in [CORE, RBAC] {
        assert!(!template.contains("kind: Secret"));
        assert!(!template.contains("stringData"));
        assert!(!template.contains("data:\n  token"));
    }
}

#[test]
fn deployment_is_pinned_non_root_and_probed() {
    let docs = render_core("serum-proxy");
    let pod = &by_kind(&docs, "Deployment")["spec"]["template"]["spec"];
    let container = container(&docs);

    assert_eq!(container["image"], IMAGE);
    assert_eq!(pod["securityContext"]["runAsNonRoot"], true);
    // The image declares USER by name, so the uid has to be explicit or the
    // kubelet cannot enforce runAsNonRoot.
    assert_eq!(pod["securityContext"]["runAsUser"], 10001);
    assert_eq!(
        container["securityContext"]["allowPrivilegeEscalation"],
        false
    );
    assert_eq!(container["securityContext"]["readOnlyRootFilesystem"], true);

    let probe = &container["livenessProbe"]["httpGet"];
    assert_eq!(probe["path"], "/healthz");
    assert_eq!(probe["port"], 8088);
}

#[test]
fn service_account_follows_the_connector_selection() {
    let with_rbac = render_core("serum-proxy");
    let pod = &by_kind(&with_rbac, "Deployment")["spec"]["template"]["spec"];
    assert_eq!(
        pod["serviceAccountName"],
        by_kind(&render_rbac(), "ServiceAccount")["metadata"]["name"],
    );

    // Without the rbac file that ServiceAccount does not exist, and a pod
    // naming a missing one never starts.
    let without_rbac = render_core("default");
    let pod = &by_kind(&without_rbac, "Deployment")["spec"]["template"]["spec"];
    assert_eq!(pod["serviceAccountName"], "default");
}

#[test]
fn rbac_resources_live_only_in_the_rbac_file() {
    assert_eq!(
        kinds(&render_rbac()),
        ["ServiceAccount", "ClusterRole", "ClusterRoleBinding"]
    );
    for kind in kinds(&render_core("serum-proxy")) {
        assert!(!kind.starts_with("ClusterRole") && kind != "ServiceAccount");
    }
}

#[test]
fn cluster_role_is_read_only_and_names_no_secrets_or_config_maps() {
    let docs = render_rbac();
    let rules = by_kind(&docs, "ClusterRole")["rules"]
        .as_sequence()
        .expect("rules is a sequence");

    for rule in rules {
        for verb in rule["verbs"].as_sequence().expect("verbs is a sequence") {
            let verb = verb.as_str().expect("verb is a string");
            assert!(matches!(verb, "get" | "list"), "write verb granted: {verb}");
        }
        for resource in rule["resources"]
            .as_sequence()
            .expect("resources is a sequence")
        {
            let resource = resource.as_str().expect("resource is a string");
            assert!(
                !matches!(resource, "secrets" | "configmaps" | "*"),
                "resource granted that must not be: {resource}"
            );
        }
    }
}

#[test]
fn cluster_scoped_names_follow_the_namespace() {
    for namespace in ["serum", "serum-staging"] {
        let docs = render_rbac_in(namespace);
        for kind in ["ClusterRole", "ClusterRoleBinding"] {
            assert_eq!(
                by_kind(&docs, kind)["metadata"]["name"].as_str(),
                Some(format!("{namespace}-proxy-read").as_str()),
            );
        }
        assert_eq!(
            by_kind(&docs, "ClusterRoleBinding")["roleRef"]["name"],
            by_kind(&docs, "ClusterRole")["metadata"]["name"],
        );
    }
}

#[test]
fn cluster_scoped_objects_carry_the_teardown_label() {
    // Deleting the namespace does not reach them; the documented teardown
    // selects on this label instead.
    let docs = render_rbac();
    for doc in &docs {
        if doc["metadata"]["namespace"].is_null() {
            assert_eq!(
                doc["metadata"]["labels"]["app.kubernetes.io/name"], "serum-proxy",
                "unlabelled cluster-scoped object: {:?}",
                doc["kind"],
            );
        }
    }
}

#[test]
fn cluster_role_binding_points_at_the_service_account() {
    let docs = render_rbac();
    let binding = by_kind(&docs, "ClusterRoleBinding");
    let sa = by_kind(&docs, "ServiceAccount");

    assert_eq!(binding["roleRef"]["kind"], "ClusterRole");
    assert_eq!(
        binding["roleRef"]["name"],
        by_kind(&docs, "ClusterRole")["metadata"]["name"]
    );
    assert_eq!(binding["subjects"][0]["name"], sa["metadata"]["name"]);
    assert_eq!(
        binding["subjects"][0]["namespace"],
        sa["metadata"]["namespace"]
    );
}

//! Configuration loading with `${VAR}` env interpolation.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// Loopback so a config that says nothing keeps the admin port off-box.
const DEFAULT_ADMIN_BIND: &str = "127.0.0.1:8088";
const ADMIN_BIND_ENV: &str = "SERUM_ADMIN_BIND";
const DEFAULT_RECONNECT_MAX_SECS: u64 = 60;
const DEFAULT_MAX_CONCURRENT: usize = 10;
const DEFAULT_LOG_LEVEL: &str = "INFO";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("env var {0} referenced in config is not set")]
    MissingEnv(String),
    #[error("YAML parse error: {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub proxy: ProxySettings,
    pub services: Vec<ServiceConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxySettings {
    pub serum_wss_url: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// No serde default: `resolve_admin_bind` fills the key first when the YAML omits it.
    pub admin_bind: String,
    #[serde(default = "default_reconnect_max")]
    pub reconnect_max_backoff_secs: u64,
    /// Permit a ws:// (no TLS) `serum_wss_url`. Off by default, and serum will not serve non-TLS.
    /// Convenience for dev in local or docker network.
    #[serde(default)]
    pub allow_insecure_ws: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    Prometheus,
    Elasticsearch,
    Kubernetes,
}

impl fmt::Display for ServiceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.serialize(f)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    #[serde(rename = "type")]
    pub kind: ServiceKind,
    pub base_url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_requests: usize,
    #[serde(default)]
    pub path_allowlist: Option<Vec<String>>,
    /// Path to a PEM CA bundle the backend's TLS cert is verified against. When set,
    /// requests to this service use a per-service client trusting this CA; otherwise
    /// the shared client trusting public roots.
    #[serde(default)]
    pub tls_ca_cert: Option<String>,
    /// Path to a file holding a bearer token, injected as Authorization and
    /// re-read on a TTL (see `TokenSource`).
    #[serde(default)]
    pub auth_token_file: Option<String>,
}

fn default_log_level() -> String {
    DEFAULT_LOG_LEVEL.into()
}
fn default_reconnect_max() -> u64 {
    DEFAULT_RECONNECT_MAX_SECS
}
fn default_max_concurrent() -> usize {
    DEFAULT_MAX_CONCURRENT
}

impl Config {
    /// Read YAML from `path`, interpolate `${VAR}` against process env, validate, return typed `Config`.
    ///
    /// Thin wrapper around `from_yaml_str` that uses `std::env::var` as the env lookup.
    /// Use `from_yaml_str` directly to inject a fake env for tests.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let path_str = path.as_ref().display().to_string();
        let raw = std::fs::read_to_string(&path).map_err(|e| ConfigError::Read {
            path: path_str,
            source: e,
        })?;
        Self::from_yaml_str(&raw, |k| std::env::var(k).ok())
    }

    /// Parse YAML with `${VAR}` interpolation; `lookup` resolves env vars.
    /// Separate entry point so tests can inject a fake env.
    /// `lookup` also supplies `SERUM_ADMIN_BIND` (see `resolve_admin_bind`).
    ///
    /// Interpolation runs against string *values* in the parsed YAML tree
    /// rather than the raw text, so `${VAR}` inside YAML comments is
    /// correctly ignored.
    pub fn from_yaml_str<F>(raw: &str, lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut value: serde_yaml::Value = serde_yaml::from_str(raw)?;
        interpolate_value(&mut value, &lookup)?;
        resolve_admin_bind(&mut value, &lookup);
        let cfg: Config = serde_yaml::from_value(value)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Post-parse semantic checks: URL protocol, one service per type, `max_concurrent_requests > 0`.
    fn validate(&self) -> Result<(), ConfigError> {
        let url = &self.proxy.serum_wss_url;
        let scheme_ok =
            url.starts_with("wss://") || (self.proxy.allow_insecure_ws && url.starts_with("ws://"));
        if !scheme_ok {
            return Err(ConfigError::Invalid(
                "proxy.serum_wss_url must start with wss:// (or ws:// when proxy.allow_insecure_ws is true)".into(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for s in &self.services {
            if !seen.insert(s.kind) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate service type {}",
                    s.kind
                )));
            }
            if s.max_concurrent_requests == 0 {
                return Err(ConfigError::Invalid(format!(
                    "service {}: max_concurrent_requests must be > 0",
                    s.kind
                )));
            }
            // tls_ca_cert requires https; https without a CA defaults to public roots.
            if s.tls_ca_cert.is_some() && !s.base_url.starts_with("https://") {
                return Err(ConfigError::Invalid(format!(
                    "service {}: tls_ca_cert is set but base_url is not https",
                    s.kind
                )));
            }
            // A bearer token over plaintext http would leak; require https.
            if s.auth_token_file.is_some() && !s.base_url.starts_with("https://") {
                return Err(ConfigError::Invalid(format!(
                    "service {}: auth_token_file is set but base_url is not https",
                    s.kind
                )));
            }
        }
        Ok(())
    }
}

/// Fill in an absent `proxy.admin_bind`: `SERUM_ADMIN_BIND` if set, else loopback.
/// Our container image sets that var to `0.0.0.0:8088`.
/// Not a serde default so the env lookup stays injectable.
fn resolve_admin_bind<F>(value: &mut serde_yaml::Value, lookup: &F)
where
    F: Fn(&str) -> Option<String>,
{
    // A config with no `proxy` mapping is rejected by the parse that follows.
    let Some(proxy) = value.get_mut("proxy").and_then(|p| p.as_mapping_mut()) else {
        return;
    };
    let key = serde_yaml::Value::from("admin_bind");
    if proxy.contains_key(&key) {
        return;
    }
    let bind = lookup(ADMIN_BIND_ENV).unwrap_or_else(|| DEFAULT_ADMIN_BIND.to_string());
    proxy.insert(key, serde_yaml::Value::from(bind));
}

/// Walk a parsed YAML tree and substitute `${VAR}` in every string leaf.
fn interpolate_value<F>(value: &mut serde_yaml::Value, lookup: &F) -> Result<(), ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let re = Regex::new(r"\$\{([A-Z_][A-Z0-9_]*)\}").expect("static regex");
    interpolate_walk(value, &re, lookup)
}

/// Recursive worker for `interpolate_value`: substitute `${VAR}` in the parsed tree.
///
/// Walks sequences and mappings; leaves non-string scalars unchanged. Returns `MissingEnv(key)` on the
/// first unresolved variable so the operator gets a named error rather than a silent empty string.
fn interpolate_walk<F>(
    value: &mut serde_yaml::Value,
    re: &Regex,
    lookup: &F,
) -> Result<(), ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    match value {
        serde_yaml::Value::String(s) => {
            let mut missing: Option<String> = None;
            let replaced = re.replace_all(s, |caps: &regex::Captures<'_>| {
                let key = &caps[1];
                match lookup(key) {
                    Some(v) => v,
                    None => {
                        if missing.is_none() {
                            missing = Some(key.to_string());
                        }
                        String::new()
                    }
                }
            });
            if let Some(k) = missing {
                return Err(ConfigError::MissingEnv(k));
            }
            *s = replaced.into_owned();
            Ok(())
        }
        serde_yaml::Value::Sequence(seq) => {
            for item in seq {
                interpolate_walk(item, re, lookup)?;
            }
            Ok(())
        }
        serde_yaml::Value::Mapping(map) => {
            for (_k, v) in map {
                interpolate_walk(v, re, lookup)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

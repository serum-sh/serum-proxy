//! Execute one `RequestFrame` against a configured backend.
//!
//! Order: strip Authorization -> build URL (path check) -> admit (path, method,
//! concurrency) -> outbound reqwest call -> stream response capped at 5 MB -> project one
//! `RequestOutcome` into an egress entry and the returned `ResponseFrame`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::Context;
use bytes::BytesMut;
use futures_util::StreamExt;
use reqwest::{Client, Method, Url};
use tracing::{Instrument, info, info_span, warn};

use crate::config::ServiceConfig;
use crate::egress::{EgressBuffer, EgressEntry, now_millis, strip_query};
use crate::envelope::{HttpMethod, QueryValue, RequestFrame, ResponseFrame};
use crate::error::ProxyError;
use crate::guardrails::Guardrails;

pub const MAX_RESPONSE_BYTES: u64 = 5 * 1024 * 1024;
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(15);

pub struct ResolvedService {
    pub config: ServiceConfig,
    pub guardrails: Guardrails,
    /// Per-service client trusting `config.tls_ca_cert`'s CA, built once at startup.
    /// `None` when no CA is configured, in which case requests use the shared client.
    pub tls_client: Option<Client>,
    /// Bearer-token source from `config.auth_token_file`; `None` when unset, in
    /// which case requests carry no injected auth.
    pub token_source: Option<TokenSource>,
}

impl ResolvedService {
    /// Assemble a service from its config: compile the guardrails, build the
    /// optional per-service TLS client, and build the optional token source.
    /// Clones `config` into the struct.
    pub fn from_config(config: &ServiceConfig) -> anyhow::Result<Self> {
        let guardrails = Guardrails::from_service(config)
            .with_context(|| format!("compiling allowlist for service {}", config.kind))?;
        let tls_client = build_service_client(config)?;
        let token_source = build_token_source(config)?;
        Ok(Self {
            config: config.clone(),
            guardrails,
            tls_client,
            token_source,
        })
    }

    /// Pick the outbound client: the per-service CA-trusting client when this service
    /// configured `tls_ca_cert`, else the shared `shared` client trusting public roots.
    pub fn client<'a>(&'a self, shared: &'a Client) -> &'a Client {
        self.tls_client.as_ref().unwrap_or(shared)
    }
}

pub struct Executor {
    client: Client,
    egress: EgressBuffer,
}

impl Executor {
    pub fn new(client: Client, egress: EgressBuffer) -> Self {
        Self { client, egress }
    }

    /// Run one inbound `RequestFrame` to a `ResponseFrame`: admit -> forward ->
    /// project one `RequestOutcome` into the egress row and the wire response.
    pub async fn execute(&self, mut req: RequestFrame, service: &ResolvedService) -> ResponseFrame {
        // Drop incoming Authorization before any further work; auth attaches from
        // local config (docs/protocol.md, request frame).
        if let Some(headers) = req.headers.as_mut() {
            headers.retain(|k, _| !k.eq_ignore_ascii_case("authorization"));
        }

        let span = info_span!(
            "execute",
            request_id = %req.request_id,
            target_service = %req.target_service,
            method = req.method.as_str(),
            path = %req.path,
        );
        let outcome = self
            .forward(&req, service)
            .instrument(span.clone())
            .await
            .unwrap_or_else(RequestOutcome::Failed);

        if let RequestOutcome::Failed(err) = &outcome {
            let frame_error = err.to_frame_error();
            // In the span: the line gets the request fields, as "request completed" does.
            span.in_scope(|| {
                warn!(
                    error_code = ?frame_error.code,
                    "request rejected: {}",
                    frame_error.message
                );
            });
        }

        // Project the single outcome into both records. Egress only borrows; the
        // response consumes it (moves the body out without a copy), so push first.
        self.egress.push(outcome.to_egress(&req));
        outcome.to_response(req.request_id)
    }

    /// Forward an admitted request to the backend and read the capped response.
    ///
    /// Holds the concurrency permit for the whole call. Each `?` here means no
    /// backend response was produced; `execute` maps the error to
    /// `RequestOutcome::Failed`.
    async fn forward(
        &self,
        req: &RequestFrame,
        service: &ResolvedService,
    ) -> Result<RequestOutcome, ProxyError> {
        let url = build_url(&service.config, &req.path, req.query.as_ref())?;
        let _permit = service.guardrails.admit(&req.method, &req.path).await?;

        let client = service.client(&self.client);
        let mut builder = client.request(method_reqwest(&req.method), url);

        if let Some(incoming) = &req.headers {
            for (k, v) in incoming {
                builder = builder.header(k, v);
            }
        }
        for (k, v) in &service.config.headers {
            builder = builder.header(k, v);
        }
        // Attach the bearer token; the incoming Authorization was stripped in `execute`.
        if let Some(ts) = &service.token_source {
            builder = builder.header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", ts.token()),
            );
        }
        if let Some(b) = &req.body {
            builder = builder.body(b.clone());
        }

        let resp = builder.send().await.map_err(classify_reqwest_error)?;

        let status = resp.status().as_u16();
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in resp.headers().iter() {
            if let Ok(s) = v.to_str() {
                headers.insert(k.to_string(), s.to_string());
            }
        }

        let mut stream = resp.bytes_stream();
        let mut buf = BytesMut::new();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(classify_reqwest_error)?;
            if (buf.len() as u64) + (bytes.len() as u64) > MAX_RESPONSE_BYTES {
                return Err(ProxyError::BodyLimitExceeded {
                    limit: MAX_RESPONSE_BYTES,
                });
            }
            buf.extend_from_slice(&bytes);
        }

        let bytes_len = buf.len() as u64;
        let body = String::from_utf8_lossy(&buf).into_owned();
        info!(status, bytes = bytes_len, "request completed");

        Ok(RequestOutcome::Response {
            status,
            headers,
            body,
            bytes: bytes_len,
        })
    }
}

/// What one admitted request produced: a backend response, or a failure.
///
/// The single source of truth for the two records every executed request emits.
/// `to_response` builds the wire `ResponseFrame`; `to_egress` builds the
/// transparency-panel row. Deriving both from one value is the point: the response
/// and the egress entry cannot disagree about status or error code.
pub enum RequestOutcome {
    /// Backend responded. `bytes` is the raw streamed length; `body` is its lossy
    /// UTF-8 rendering, so the two lengths can differ.
    Response {
        status: u16,
        headers: BTreeMap<String, String>,
        body: String,
        bytes: u64,
    },
    /// Admission denied (403/405/429) or the upstream call failed (502/504/413).
    Failed(ProxyError),
}

impl RequestOutcome {
    /// Build the single-chunk wire response. Consumes the outcome to move the body
    /// out without copying.
    pub fn to_response(self, request_id: String) -> ResponseFrame {
        match self {
            Self::Response {
                status,
                headers,
                body,
                ..
            } => ResponseFrame::reply(request_id, status, headers, body),
            Self::Failed(err) => ResponseFrame::error(request_id, err.to_frame_error()),
        }
    }

    /// Build the egress row: identity fields from `req`, result fields from the outcome.
    ///
    /// Every outcome gets a row, also a request that never reached the backend: a guardrail
    /// denial (403 path, 405 method, 429 concurrency) or a `build_url` error. `session.rs`
    /// rejects an unknown service or a bad `request_id` before the executor, with no row.
    pub fn to_egress(&self, req: &RequestFrame) -> EgressEntry {
        let (status, response_bytes, error_code) = match self {
            Self::Response { status, bytes, .. } => (Some(*status), Some(*bytes), None),
            Self::Failed(err) => (None, None, Some(format!("{:?}", err.to_frame_error().code))),
        };
        EgressEntry {
            timestamp_ms: now_millis(),
            request_id: req.request_id.clone(),
            target_service: req.target_service.clone(),
            method: req.method.as_str().into(),
            path: strip_query(&req.path),
            status,
            response_bytes,
            redaction_summary: None,
            error_code,
        }
    }
}

fn method_reqwest(m: &HttpMethod) -> Method {
    match m {
        HttpMethod::Get => Method::GET,
        HttpMethod::Post => Method::POST,
    }
}

/// Build the outbound URL using `urlencode(..., doseq=True)` semantics, and reject an unsafe
/// `path`.
///
/// A list-valued query entry becomes one `name=value` pair per item
/// (`match[]=a&match[]=b`), not a single stringified list. See `docs/protocol.md`,
/// "Query serialization".
///
/// Rejects with `AllowlistDeny` a `path` that URL parsing changes (e.g. `..`, `%2e%2e`, `\`,
/// `?`, `#`, a space) or that has `%2F`, `%5C` or an empty segment. On success, the URL path
/// after the base path (e.g. `/prometheus`) is exactly `path`.
pub fn build_url(
    config: &ServiceConfig,
    path: &str,
    query: Option<&BTreeMap<String, QueryValue>>,
) -> Result<Url, ProxyError> {
    let parse =
        |s: &str| Url::parse(s).map_err(|e| ProxyError::Internal(format!("invalid URL {s}: {e}")));
    let base = parse(&config.base_url)?;
    let base_path = base.path().trim_end_matches('/');
    let mut url = parse(&format!("{}{path}", config.base_url.trim_end_matches('/')))?;

    // A path with no leading `/` can change the authority (`@other-host/...`). The equality
    // check also rejects it; the guard makes the rule explicit.
    let unchanged = path.starts_with('/')
        && url.path().strip_prefix(base_path) == Some(path)
        && url.query().is_none()
        && url.fragment().is_none();
    // The `url` crate keeps `%2F` and `%5C`, but a backend (e.g. the Kubernetes API server) can
    // decode them and route on a path that the allowlist did not check.
    let lower = path.to_ascii_lowercase();
    let encoded_separator = lower.contains("%2f") || lower.contains("%5c");
    // An allowlist `*` matches an empty segment, and the Kubernetes API server reads
    // `/namespaces//pods` as all namespaces.
    let empty_segment = path.split('/').skip(1).any(str::is_empty);
    if !unchanged || encoded_separator || empty_segment {
        return Err(ProxyError::AllowlistDeny {
            service: config.kind.to_string(),
            path: path.to_string(),
        });
    }

    if let Some(map) = query {
        let mut pairs = url.query_pairs_mut();
        for (k, v) in map {
            match v {
                QueryValue::One(s) => {
                    pairs.append_pair(k, s);
                }
                QueryValue::Many(items) => {
                    for item in items {
                        pairs.append_pair(k, item);
                    }
                }
            }
        }
    }
    Ok(url)
}

/// Classify a reqwest send/stream error into the proxy's typed error set.
///
/// Timeouts get `504_TIMEOUT`; connection-level failures (refused, DNS, TLS
/// handshake) get `502_BACKEND_UNREACHABLE`. Anything else falls through to
/// `Upstream` which also maps to `502_BACKEND_UNREACHABLE` via `to_frame_error`.
fn classify_reqwest_error(e: reqwest::Error) -> ProxyError {
    if e.is_timeout() {
        ProxyError::UpstreamTimeout
    } else if e.is_connect() {
        ProxyError::BackendUnreachable {
            message: format!("connection failed: {e}"),
        }
    } else {
        ProxyError::Upstream { source: e }
    }
}

/// Base client config shared by the global client and any per-service client:
/// 15s timeout, no redirects. The per-service builder adds a private CA on top.
fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(UPSTREAM_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
}

/// Build the shared `reqwest::Client`.
pub fn build_client() -> reqwest::Client {
    client_builder()
        .build()
        .expect("reqwest client builds with rustls and the configured features")
}

/// Build a per-service client trusting the PEM CA bundle at `config.tls_ca_cert`,
/// or `None` when the service configured no CA (callers fall back to the shared
/// client). The bundle may hold multiple certs; each is added as a root on top of
/// the public roots. A bundle with no certificate is rejected.
pub fn build_service_client(svc: &ServiceConfig) -> anyhow::Result<Option<Client>> {
    let Some(path) = &svc.tls_ca_cert else {
        return Ok(None);
    };
    let pem = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("service {}: reading tls_ca_cert {path}: {e}", svc.kind))?;
    let certs = reqwest::Certificate::from_pem_bundle(&pem)
        .map_err(|e| anyhow::anyhow!("service {}: parsing tls_ca_cert {path}: {e}", svc.kind))?;
    if certs.is_empty() {
        anyhow::bail!(
            "service {}: tls_ca_cert {path} contained no certificates",
            svc.kind
        );
    }
    let mut builder = client_builder();
    for cert in certs {
        builder = builder.add_root_certificate(cert);
    }
    let client = builder
        .build()
        .map_err(|e| anyhow::anyhow!("service {}: building TLS client: {e}", svc.kind))?;
    Ok(Some(client))
}

/// A bearer token read from a file, cached and re-read once older than `ttl`.
///
/// The kubelet periodically rotates the ServiceAccount token file, so `ttl` is
/// kept well under the token lifetime to avoid serving a stale token.
pub struct TokenSource {
    service: String,
    path: PathBuf,
    ttl: Duration,
    cached: RwLock<CachedToken>,
}

struct CachedToken {
    token: String,
    fetched_at: Instant,
}

impl TokenSource {
    /// The current bearer token, re-read from disk when the cached copy expires.
    ///
    /// Never fails: a re-read error keeps the still-valid cached token and is
    /// logged, so a transient FS error does not break in-flight requests. The
    /// read is synchronous but tiny.
    pub fn token(&self) -> String {
        {
            let guard = self.cached.read().expect("token cache lock not poisoned");
            if guard.fetched_at.elapsed() < self.ttl {
                return guard.token.clone();
            }
        }
        let mut guard = self.cached.write().expect("token cache lock not poisoned");
        // Re-check: a concurrent caller may have refreshed while we waited for
        // the lock, so only the first re-reads.
        if guard.fetched_at.elapsed() >= self.ttl {
            match read_token(&self.path, &self.service) {
                Ok(token) => {
                    guard.token = token;
                    guard.fetched_at = Instant::now();
                }
                Err(e) => {
                    warn!(
                        service = %self.service,
                        error = %e,
                        "auth_token_file re-read failed; serving cached token"
                    );
                    // Back off to one retry per `ttl` instead of hammering a failing file.
                    guard.fetched_at = Instant::now();
                }
            }
        }
        guard.token.clone()
    }
}

/// Default re-read interval, long enough to avoid re-reading per request.
pub const TOKEN_REREAD_TTL: Duration = Duration::from_secs(60);

/// Build a [`TokenSource`] from `auth_token_file`, or `None` when unset. Reads
/// the token once so a missing or empty file fails fast at startup.
pub fn build_token_source(svc: &ServiceConfig) -> anyhow::Result<Option<TokenSource>> {
    build_token_source_with_ttl(svc, TOKEN_REREAD_TTL)
}

/// [`build_token_source`] with an explicit `ttl`; tests use a short one to cross
/// the re-read boundary quickly.
pub fn build_token_source_with_ttl(
    svc: &ServiceConfig,
    ttl: Duration,
) -> anyhow::Result<Option<TokenSource>> {
    let Some(path) = &svc.auth_token_file else {
        return Ok(None);
    };
    let token = read_token(Path::new(path), &svc.kind.to_string())?;
    Ok(Some(TokenSource {
        service: svc.kind.to_string(),
        path: PathBuf::from(path),
        ttl,
        cached: RwLock::new(CachedToken {
            token,
            fetched_at: Instant::now(),
        }),
    }))
}

/// Read and trim a token from `path`; reject an empty file so a misconfig fails
/// loudly instead of sending an empty Authorization.
fn read_token(path: &Path, service: &str) -> anyhow::Result<String> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        anyhow::anyhow!(
            "service {service}: reading auth_token_file {}: {e}",
            path.display()
        )
    })?;
    let token = raw.trim().to_string();
    if token.is_empty() {
        anyhow::bail!(
            "service {service}: auth_token_file {} is empty",
            path.display()
        );
    }
    Ok(token)
}

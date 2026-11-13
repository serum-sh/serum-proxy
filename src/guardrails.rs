//! Pre-flight safety checks: path and method allowlist, concurrency cap.
//!
//! Body-size cap is enforced in `executor` against the streamed response.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::{ServiceConfig, ServiceKind};
use crate::envelope::HttpMethod;
use crate::error::ProxyError;

/// One allowlist entry: the method and the path glob of one read that serum sends.
pub type AllowEntry = (HttpMethod, &'static str);

const PROMETHEUS_ALLOWLIST: &[AllowEntry] = &[
    (HttpMethod::Get, "/api/v1/query"),
    (HttpMethod::Get, "/api/v1/query_range"),
    (HttpMethod::Get, "/api/v1/series"),
    (HttpMethod::Get, "/api/v1/label/*/values"),
    (HttpMethod::Get, "/api/v1/metadata"),
];

// `POST /*/_mapping` is the put mapping API (a write), so `/*/_mapping` is GET only.
const ELASTICSEARCH_ALLOWLIST: &[AllowEntry] = &[
    (HttpMethod::Get, "/_cat/indices/*"),
    (HttpMethod::Get, "/*/_mapping"),
    (HttpMethod::Post, "/*/_search"),
];

// GET only, so the proxy keeps Kubernetes read-only.
const KUBERNETES_ALLOWLIST: &[AllowEntry] = &[
    (HttpMethod::Get, "/api/v1/namespaces/*/pods"),
    (HttpMethod::Get, "/api/v1/namespaces/*/events"),
];

/// Built-in allowlist of `kind`. No config can add a path or a method to it.
pub fn default_allowlist(kind: ServiceKind) -> &'static [AllowEntry] {
    match kind {
        ServiceKind::Prometheus => PROMETHEUS_ALLOWLIST,
        ServiceKind::Elasticsearch => ELASTICSEARCH_ALLOWLIST,
        ServiceKind::Kubernetes => KUBERNETES_ALLOWLIST,
    }
}

/// Compile a glob set where `*` matches one path segment (it does not cross `/`).
fn build_glob_set<'a>(
    patterns: impl IntoIterator<Item = &'a str>,
) -> Result<GlobSet, globset::Error> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        builder.add(GlobBuilder::new(p).literal_separator(true).build()?);
    }
    builder.build()
}

/// Entries of `paths` that overlap no built-in entry, so they allow nothing.
fn unused_entries(paths: &[String], allowlist: &[AllowEntry]) -> Vec<String> {
    paths
        .iter()
        .filter(|p| !allowlist.iter().any(|(_, b)| globs_overlap(p, b)))
        .cloned()
        .collect()
}

/// Two path globs overlap if they have the same number of segments and, in each segment, one
/// glob matches the text of the other. Misses rare overlaps (e.g. `logs-*` and `*-2025` in one
/// segment, or `**`). A segment that is not a valid glob counts as a match.
fn globs_overlap(a: &str, b: &str) -> bool {
    let matches = |glob: &str, text: &str| {
        Glob::new(glob).map_or(true, |g| g.compile_matcher().is_match(text))
    };
    let (a, b): (Vec<&str>, Vec<&str>) = (a.split('/').collect(), b.split('/').collect());
    a.len() == b.len()
        && a.iter()
            .zip(&b)
            .all(|(x, y)| x == y || matches(x, y) || matches(y, x))
}

pub struct Guardrails {
    pub service_name: String,
    /// Built-in entries; entry `i` is pattern `i` of `allowlist_set`.
    pub allowlist: &'static [AllowEntry],
    pub allowlist_set: GlobSet,
    /// Operator `path_allowlist`. If set, a path must also match it to pass.
    pub path_allowlist_set: Option<GlobSet>,
    /// `path_allowlist` entries that allow nothing, because no built-in entry overlaps them.
    pub unused_path_allowlist: Vec<String>,
    pub semaphore: Arc<Semaphore>,
    pub max_permits: usize,
    pub queue_ceiling: usize,
    /// Total in-flight requests (executing + waiting on the semaphore).
    /// Bumped on entry to `acquire_with_ceiling`; decremented when the
    /// returned `CeilingPermit` drops.
    in_flight: Arc<AtomicUsize>,
}

/// Guard around an active permit; restores the in-flight counter on drop.
///
/// Must outlive the upstream call: dropping it frees the concurrency slot.
#[must_use = "dropping the permit frees the concurrency slot; hold it for the call"]
pub struct CeilingPermit {
    _permit: OwnedSemaphorePermit,
    in_flight: Arc<AtomicUsize>,
}

impl Drop for CeilingPermit {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Guardrails {
    /// Compile the built-in allowlist and the operator `path_allowlist`, and set up the
    /// per-service concurrency gate.
    pub fn from_service(svc: &ServiceConfig) -> Result<Self, globset::Error> {
        let allowlist = default_allowlist(svc.kind);
        let allowlist_set = build_glob_set(allowlist.iter().map(|(_, path)| *path))?;
        let path_allowlist_set = svc
            .path_allowlist
            .as_ref()
            .map(|paths| build_glob_set(paths.iter().map(String::as_str)))
            .transpose()?;
        let unused_path_allowlist = svc
            .path_allowlist
            .as_deref()
            .map(|paths| unused_entries(paths, allowlist))
            .unwrap_or_default();
        Ok(Self {
            service_name: svc.kind.to_string(),
            allowlist,
            allowlist_set,
            path_allowlist_set,
            unused_path_allowlist,
            semaphore: Arc::new(Semaphore::new(svc.max_concurrent_requests)),
            max_permits: svc.max_concurrent_requests,
            queue_ceiling: 2 * svc.max_concurrent_requests,
            in_flight: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Admit one request through the guardrails in precedence order: path
    /// (403) then method (405) then concurrency (429). Returns the held permit;
    /// keep it alive for the duration of the upstream call.
    ///
    /// The order is the contract: a forbidden request is rejected before any
    /// permit is taken, so a denied request never counts against the cap.
    ///
    /// `path` must be the path that the backend gets; see `executor::build_url`.
    pub async fn admit(
        &self,
        method: &HttpMethod,
        path: &str,
    ) -> Result<CeilingPermit, ProxyError> {
        self.check(method, path)?;
        self.acquire_with_ceiling().await
    }

    /// Pure pre-flight policy. Private; `admit` owns the order.
    ///
    /// 403 if no built-in entry matches `path`, or if `path_allowlist` is set and does not match
    /// it. 405 if all built-in entries that match `path` have a different method.
    fn check(&self, method: &HttpMethod, path: &str) -> Result<(), ProxyError> {
        let hits = self.allowlist_set.matches(path);
        let operator_allows = self
            .path_allowlist_set
            .as_ref()
            .is_none_or(|set| set.is_match(path));
        if hits.is_empty() || !operator_allows {
            return Err(ProxyError::AllowlistDeny {
                service: self.service_name.clone(),
                path: path.to_string(),
            });
        }
        if hits.iter().any(|&i| self.allowlist[i].0 == *method) {
            Ok(())
        } else {
            Err(ProxyError::MethodNotAllowed {
                method: method.as_str().into(),
            })
        }
    }

    /// Acquire a permit. Up to `queue_ceiling` requests may be in the system
    /// (executing or waiting); the next one is rejected with 429.
    async fn acquire_with_ceiling(&self) -> Result<CeilingPermit, ProxyError> {
        // Reserve our slot first so the queue depth check is atomic with the
        // increment that puts us into the queue.
        let prev = self.in_flight.fetch_add(1, Ordering::SeqCst);
        if prev + 1 > self.queue_ceiling {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            return Err(ProxyError::ConcurrencyExceeded {
                service: self.service_name.clone(),
            });
        }
        match self.semaphore.clone().acquire_owned().await {
            Ok(permit) => Ok(CeilingPermit {
                _permit: permit,
                in_flight: self.in_flight.clone(),
            }),
            Err(_) => {
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                Err(ProxyError::ConcurrencyExceeded {
                    service: self.service_name.clone(),
                })
            }
        }
    }

    /// In-flight count (executing + queued). Internal seam: unused in
    /// production; the concurrency-cap tests read it to observe queue depth.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }
}

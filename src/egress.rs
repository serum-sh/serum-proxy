//! In-memory ring buffer of recently forwarded requests.
//!
//! Operator-facing transparency artifact. Response bodies
//! are explicitly NOT retained.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::envelope::RedactionSummary;

pub const DEFAULT_CAPACITY: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct EgressEntry {
    pub timestamp_ms: u64,
    pub request_id: String,
    pub target_service: String,
    pub method: String,
    /// Path with query stripped (or `?<redacted>` placeholder).
    pub path: String,
    pub status: Option<u16>,
    pub response_bytes: Option<u64>,
    pub redaction_summary: Option<RedactionSummary>,
    pub error_code: Option<String>,
}

#[derive(Clone)]
pub struct EgressBuffer {
    inner: Arc<Mutex<VecDeque<EgressEntry>>>,
    capacity: usize,
}

impl EgressBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    /// Append one entry; evict the oldest if the buffer is at capacity.
    pub fn push(&self, entry: EgressEntry) {
        let mut buf = self.inner.lock().expect("egress mutex poisoned");
        if buf.len() == self.capacity {
            buf.pop_front();
        }
        buf.push_back(entry);
    }

    /// Return all entries newest-first, suitable for the `/egress` JSON view.
    pub fn snapshot(&self) -> Vec<EgressEntry> {
        let buf = self.inner.lock().expect("egress mutex poisoned");
        buf.iter().rev().cloned().collect()
    }
}

/// Wall-clock ms since `UNIX_EPOCH`; falls back to 0.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Drop the query string from `?` onward for the panel; preserves the path.
pub fn strip_query(path: &str) -> String {
    match path.find('?') {
        Some(i) => format!("{}?<redacted>", &path[..i]),
        None => path.to_string(),
    }
}

//! Internal proxy errors and their mapping to wire-level `FrameError`s.

use thiserror::Error;

use crate::envelope::{ErrorCode, FrameError};

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("path {path} not in allowlist for service {service}")]
    AllowlistDeny { service: String, path: String },

    #[error("unknown target_service {service}")]
    UnknownService { service: String },

    #[error("method {method} not allowed for this path")]
    MethodNotAllowed { method: String },

    #[error("response exceeded {limit} bytes")]
    BodyLimitExceeded { limit: u64 },

    #[error("service {service} at concurrency ceiling; rejecting")]
    ConcurrencyExceeded { service: String },

    #[error("upstream timed out")]
    UpstreamTimeout,

    #[error("backend unreachable: {message}")]
    BackendUnreachable { message: String },

    #[error("upstream call failed: {source}")]
    Upstream {
        #[from]
        source: reqwest::Error,
    },

    #[error("internal error: {0}")]
    Internal(String),
}

impl ProxyError {
    /// Map to the typed error envelope returned to serum.
    pub fn to_frame_error(&self) -> FrameError {
        match self {
            Self::AllowlistDeny { service: _, path } => FrameError {
                code: ErrorCode::AllowlistDeny,
                message: format!("path {path} not in allowlist"),
                retryable: false,
            },
            Self::UnknownService { service } => FrameError {
                code: ErrorCode::UnknownService,
                message: format!("target_service {service} not configured in this proxy"),
                retryable: false,
            },
            Self::MethodNotAllowed { method } => FrameError {
                code: ErrorCode::MethodNotAllowed,
                message: format!("method {method} not allowed for this path"),
                retryable: false,
            },
            Self::BodyLimitExceeded { limit } => FrameError {
                code: ErrorCode::BodyLimitExceeded,
                message: format!("response exceeded {limit} bytes"),
                retryable: false,
            },
            Self::ConcurrencyExceeded { service } => FrameError {
                code: ErrorCode::ConcurrencyExceeded,
                message: format!("concurrency cap exceeded for service {service}"),
                retryable: true,
            },
            Self::UpstreamTimeout => FrameError {
                code: ErrorCode::UpstreamTimeout,
                message: "upstream did not respond within 15s".into(),
                retryable: true,
            },
            Self::BackendUnreachable { message } => FrameError {
                code: ErrorCode::BackendUnreachable,
                message: message.clone(),
                retryable: true,
            },
            Self::Upstream { source } => FrameError {
                code: ErrorCode::BackendUnreachable,
                message: format!("upstream call failed: {source}"),
                retryable: true,
            },
            Self::Internal(msg) => FrameError {
                code: ErrorCode::BackendUnreachable,
                message: format!("internal proxy error: {msg}"),
                retryable: false,
            },
        }
    }
}

//! serum proxy library surface.

// Set here, not in Cargo.toml `[lints]`, so tests can still unwrap.
#![warn(clippy::unwrap_used)]

pub mod admin;
pub mod config;
pub mod egress;
pub mod envelope;
pub mod error;
pub mod executor;
pub mod guardrails;
pub mod logging;
pub mod session;

//! Local operator HTTP server: `/healthz` and `/egress`.
//!
//! Bound to `proxy.admin_bind`; not exposed to serum.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::RwLock;

use crate::egress::EgressBuffer;
use crate::session::ConnState;

#[derive(Clone)]
pub struct SessionStatus {
    pub state: ConnState,
    pub last_frame_at: Option<Instant>,
}

impl Default for SessionStatus {
    fn default() -> Self {
        Self {
            state: ConnState::Down,
            last_frame_at: None,
        }
    }
}

pub type SharedSessionStatus = Arc<RwLock<SessionStatus>>;

#[derive(Clone)]
pub struct AdminState {
    pub session: SharedSessionStatus,
    pub egress: EgressBuffer,
    pub services: Vec<String>,
}

#[derive(Serialize)]
struct HealthzResponse<'a> {
    ws_state: &'a str,
    seconds_since_last_frame: Option<u64>,
    services: &'a [String],
}

/// GET `/healthz`: WS state, seconds since last frame, configured services.
async fn healthz(State(state): State<AdminState>) -> Json<serde_json::Value> {
    let s = state.session.read().await;
    let secs = s.last_frame_at.map(|t| t.elapsed().as_secs());
    let body = HealthzResponse {
        ws_state: state_label(s.state),
        seconds_since_last_frame: secs,
        services: &state.services,
    };
    Json(serde_json::to_value(body).expect("serialize healthz"))
}

/// GET `/egress`: returns the recent-request ring buffer as JSON (newest first).
async fn egress(State(state): State<AdminState>) -> Json<serde_json::Value> {
    let entries = state.egress.snapshot();
    Json(serde_json::to_value(entries).expect("serialize egress"))
}

fn state_label(s: ConnState) -> &'static str {
    match s {
        ConnState::Connected => "connected",
        ConnState::Reconnecting => "reconnecting",
        ConnState::Down => "down",
    }
}

pub fn router(state: AdminState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/egress", get(egress))
        .with_state(state)
}

pub async fn serve(bind: SocketAddr, state: AdminState) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    tracing::info!(%bind, "admin HTTP server listening");
    axum::serve(listener, router(state)).await
}

//! WS handshake against a mock serum server.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serum_proxy::{
    admin::{SessionStatus, SharedSessionStatus},
    egress::EgressBuffer,
    executor::{Executor, build_client},
    session::{self, ConnState},
};
use tokio::net::TcpListener;
use tokio::sync::{RwLock, watch};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{Response as HttpResponse, StatusCode};
use tokio_tungstenite::{accept_async, accept_hdr_async, tungstenite::Message};

/// Ack that matches the proxy's supported protocol version.
const ACK_V1: &str = r#"{"type":"hello","protocol_version":1}"#;

/// Shared, observable mock state for assertions that span (re)connects.
#[derive(Default)]
struct MockState {
    /// Upgrades accepted (handshake reached `accept_*`). Drives the reconnect proof.
    accepts: AtomicUsize,
    /// Upgrades rejected with 403. Proves the auth-reject path actually fired.
    rejects: AtomicUsize,
    /// Wall-clock of each accept, to measure the reconnect backoff gap.
    accept_times: Mutex<Vec<Instant>>,
    /// `Authorization` header seen on the first upgrade request.
    auth_header: Mutex<Option<String>>,
}

struct MockServer {
    url: String,
    handle: tokio::task::JoinHandle<Option<String>>,
    state: Arc<MockState>,
}

#[derive(Copy, Clone)]
enum MockBehavior {
    AckV1,
    AckV99,
    /// Capture the upgrade `Authorization` header, then ack v1.
    AssertBearer,
    /// Reject every upgrade with HTTP 403 (bad-token path).
    RejectAuth,
    /// Accept in a loop: ack v1, then hang up; lets the client prove reconnect.
    Reconnect,
}

/// Start a mock server. The behavior is set by `mode`. The loop modes
/// (`RejectAuth`, `Reconnect`) run until the returned handle is aborted.
async fn spawn_mock(mode: MockBehavior) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{addr}");
    let state = Arc::new(MockState::default());
    let task_state = state.clone();
    let handle = tokio::spawn(async move {
        match mode {
            MockBehavior::AckV1 | MockBehavior::AckV99 => serve_one_shot(listener, mode).await,
            MockBehavior::AssertBearer => serve_assert_bearer(listener, task_state).await,
            MockBehavior::RejectAuth => serve_reject_auth(listener, task_state).await,
            MockBehavior::Reconnect => serve_reconnect(listener, task_state).await,
        }
    });
    MockServer { url, handle, state }
}

/// One accept, complete the upgrade, ack per `mode`, then close. Returns the hello frame text.
async fn serve_one_shot(listener: TcpListener, mode: MockBehavior) -> Option<String> {
    let (sock, _) = listener.accept().await.ok()?;
    let mut ws = accept_async(sock).await.ok()?;
    let first = ws.next().await?.ok()?;
    let received = frame_text(&first)?;
    match mode {
        MockBehavior::AckV1 => {
            ws.send(Message::Text(ACK_V1.into())).await.ok()?;
            // Keep the socket open briefly so the client observes Connected.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = ws.close(None).await;
        }
        MockBehavior::AckV99 => {
            let ack = r#"{"type":"hello","protocol_version":99}"#;
            ws.send(Message::Text(ack.into())).await.ok()?;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        _ => unreachable!("serve_one_shot only handles AckV1/AckV99"),
    }
    Some(received)
}

/// Capture the upgrade `Authorization` header, then complete the handshake and ack v1.
#[expect(
    clippy::result_large_err,
    reason = "tungstenite Callback trait fixes the Result shape"
)]
async fn serve_assert_bearer(listener: TcpListener, state: Arc<MockState>) -> Option<String> {
    let (sock, _) = listener.accept().await.ok()?;
    let cb_state = state.clone();
    let callback = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        let header = req
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        *cb_state.auth_header.lock().unwrap() = header;
        Ok(resp)
    };
    let mut ws = accept_hdr_async(sock, callback).await.ok()?;
    let first = ws.next().await?.ok()?;
    let received = frame_text(&first)?;
    ws.send(Message::Text(ACK_V1.into())).await.ok()?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = ws.close(None).await;
    Some(received)
}

/// Reject every upgrade with HTTP 403, mirroring serum's pre-accept bad-token close.
#[expect(
    clippy::result_large_err,
    reason = "tungstenite Callback trait fixes the Result shape"
)]
async fn serve_reject_auth(listener: TcpListener, state: Arc<MockState>) -> Option<String> {
    loop {
        let Ok((sock, _)) = listener.accept().await else {
            continue;
        };
        let cb_state = state.clone();
        let callback = move |_req: &Request, _resp: Response| -> Result<Response, ErrorResponse> {
            cb_state.rejects.fetch_add(1, Ordering::Relaxed);
            let resp = HttpResponse::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Some("forbidden".to_string()))
                .unwrap();
            Err(resp)
        };
        // accept_hdr_async writes the 403 and returns Err; the client sees 403 Forbidden.
        let _ = accept_hdr_async(sock, callback).await;
    }
}

/// Accept in a loop: ack v1 so the client reaches Connected, then hang up hard.
async fn serve_reconnect(listener: TcpListener, state: Arc<MockState>) -> Option<String> {
    loop {
        let Ok((sock, _)) = listener.accept().await else {
            continue;
        };
        state.accept_times.lock().unwrap().push(Instant::now());
        state.accepts.fetch_add(1, Ordering::Relaxed);
        let Ok(mut ws) = accept_async(sock).await else {
            continue;
        };
        // Drain the hello and ack v1 so the client reaches Connected (which also
        // resets its backoff). Stay up briefly so the state is observable, then
        // drop without a close frame: a hard failure the client must back off from.
        let _ = ws.next().await;
        let _ = ws.send(Message::Text(ACK_V1.into())).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(ws);
    }
}

fn frame_text(msg: &Message) -> Option<String> {
    match msg {
        Message::Text(t) => Some(t.to_string()),
        Message::Binary(b) => Some(String::from_utf8_lossy(b).to_string()),
        _ => None,
    }
}

fn fake_session_cfg(url: &str) -> session::SessionConfig {
    session::SessionConfig {
        url: url.into(),
        token: "test-token".into(),
        heartbeat_interval: Duration::from_millis(100),
        max_backoff: Duration::from_secs(1),
        proxy_build_version: "0.1.0-test".into(),
    }
}

async fn fixtures() -> (
    Arc<session::ServiceRegistry>,
    Arc<Executor>,
    SharedSessionStatus,
    watch::Sender<bool>,
    watch::Receiver<bool>,
) {
    let registry: session::ServiceRegistry = HashMap::new();
    let exec = Arc::new(Executor::new(build_client(), EgressBuffer::new(8)));
    let status = Arc::new(RwLock::new(SessionStatus::default()));
    let (tx, rx) = watch::channel(false);
    (Arc::new(registry), exec, status, tx, rx)
}

#[tokio::test]
async fn handshake_v1_reaches_connected() {
    let mock = spawn_mock(MockBehavior::AckV1).await;
    let (registry, exec, status, shutdown_tx, shutdown_rx) = fixtures().await;

    let session_status = status.clone();
    let session_task = tokio::spawn(async move {
        let _ = session::run(
            fake_session_cfg(&mock.url),
            registry,
            exec,
            session_status,
            shutdown_rx,
        )
        .await;
    });

    // Poll for Connected state with a bounded deadline.
    let mut reached = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let s = status.read().await;
        if s.state == ConnState::Connected {
            reached = true;
            break;
        }
    }
    assert!(reached, "session never reached Connected");

    // Tear down.
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), session_task).await;

    let received = mock.handle.await.expect("mock server task ran");
    let received = received.expect("mock received a hello frame");
    assert!(received.contains("\"type\":\"hello\""));
    assert!(received.contains("\"protocol_versions_supported\""));
}

#[tokio::test]
async fn handshake_version_mismatch_does_not_connect() {
    let mock = spawn_mock(MockBehavior::AckV99).await;
    let (registry, exec, status, shutdown_tx, shutdown_rx) = fixtures().await;

    let session_status = status.clone();
    let session_task = tokio::spawn(async move {
        let _ = session::run(
            fake_session_cfg(&mock.url),
            registry,
            exec,
            session_status,
            shutdown_rx,
        )
        .await;
    });

    tokio::time::sleep(Duration::from_millis(500)).await;
    let s = status.read().await;
    assert_ne!(
        s.state,
        ConnState::Connected,
        "must not mark Connected on a version mismatch"
    );
    drop(s);

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), session_task).await;
    let _ = mock.handle.await;
}

/// The bearer token must actually cross on the upgrade request: the one auth
/// guarantee. Captured server-side, before any ack.
#[tokio::test]
async fn handshake_sends_bearer_token() {
    let mock = spawn_mock(MockBehavior::AssertBearer).await;
    let state = mock.state.clone();
    let (registry, exec, status, shutdown_tx, shutdown_rx) = fixtures().await;

    let session_task = tokio::spawn(async move {
        let _ = session::run(
            fake_session_cfg(&mock.url),
            registry,
            exec,
            status,
            shutdown_rx,
        )
        .await;
    });

    let mut captured = None;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let h = state.auth_header.lock().unwrap().clone();
        if h.is_some() {
            captured = h;
            break;
        }
    }

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), session_task).await;

    assert_eq!(
        captured.expect("mock never captured an Authorization header"),
        "Bearer test-token",
        "the configured token must be sent as a bearer credential on the upgrade"
    );
    let _ = mock.handle.await;
}

/// A rejected upgrade (bad token -> 403) must never reach Connected; the session
/// stays in the reconnect/backoff cycle instead of falsely reporting healthy.
#[tokio::test]
async fn handshake_auth_reject_never_connects() {
    let mock = spawn_mock(MockBehavior::RejectAuth).await;
    let state = mock.state.clone();
    let (registry, exec, status, shutdown_tx, shutdown_rx) = fixtures().await;

    let session_status = status.clone();
    let session_task = tokio::spawn(async move {
        let _ = session::run(
            fake_session_cfg(&mock.url),
            registry,
            exec,
            session_status,
            shutdown_rx,
        )
        .await;
    });

    // Across several reject+backoff cycles the session must never connect.
    let mut connected = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if status.read().await.state == ConnState::Connected {
            connected = true;
            break;
        }
    }
    assert!(
        !connected,
        "must not reach Connected when the upgrade is rejected"
    );
    assert!(
        state.rejects.load(Ordering::Relaxed) >= 1,
        "mock never rejected an upgrade; the 403 path was not exercised"
    );

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), session_task).await;
    mock.handle.abort();
}

/// After a hard failure (peer hangs up post-ack), the client must reconnect: a
/// second accept lands, and only after a backoff delay (not a hot retry loop).
#[tokio::test]
async fn reconnect_backoff_reconnects_after_hard_failure() {
    let mock = spawn_mock(MockBehavior::Reconnect).await;
    let state = mock.state.clone();
    let (registry, exec, status, shutdown_tx, shutdown_rx) = fixtures().await;

    let session_status = status.clone();
    let session_task = tokio::spawn(async move {
        let _ = session::run(
            fake_session_cfg(&mock.url),
            registry,
            exec,
            session_status,
            shutdown_rx,
        )
        .await;
    });

    // The first connection must come up before we can call its drop a reconnect.
    let mut connected = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if status.read().await.state == ConnState::Connected {
            connected = true;
            break;
        }
    }
    assert!(connected, "first connection never reached Connected");

    // Then wait for the reconnect: a second accept.
    let mut times = Vec::new();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        times = state.accept_times.lock().unwrap().clone();
        if times.len() >= 2 {
            break;
        }
    }

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), session_task).await;
    mock.handle.abort();

    assert!(
        times.len() >= 2,
        "expected a reconnect (>=2 accepts), saw {}",
        times.len()
    );
    let gap = times[1] - times[0];
    // The first reconnect uses the initial 500ms backoff (plus the ~200ms the mock
    // stays up). It must be delayed (backoff applied), not instant, and bounded.
    assert!(
        gap >= Duration::from_millis(400),
        "reconnect too fast; backoff not applied: {gap:?}"
    );
    assert!(
        gap <= Duration::from_millis(3000),
        "reconnect took too long: {gap:?}"
    );
}

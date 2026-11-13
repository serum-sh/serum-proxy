//! WebSocket session lifecycle: connect, hello, heartbeat, dispatch, reconnect, drain.
//!
//! One process holds one outbound WebSocket. The reader loop dispatches
//! inbound frames; per-request executor tasks send responses back through a
//! single-owner writer that drains an mpsc.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio::time::sleep;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::HeaderValue,
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};
use tracing::{debug, error, info, warn};

use crate::admin::SharedSessionStatus;
use crate::envelope::{
    ErrorCode, ErrorFrameBody, Frame, FrameError, HeartbeatFrame, HelloFrame, PROTOCOL_VERSION,
    ProtocolV1, RequestFrame, ResponseFrame, is_valid_request_id,
};
use crate::error::ProxyError;
use crate::executor::{Executor, ResolvedService};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Down,
    Reconnecting,
    Connected,
}

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

pub struct SessionConfig {
    pub url: String,
    pub token: String,
    pub heartbeat_interval: Duration,
    pub max_backoff: Duration,
    pub proxy_build_version: String,
}

pub type ServiceRegistry = HashMap<String, Arc<ResolvedService>>;

/// Delay before the first reconnect attempt. Doubles up to `max_backoff`.
const INITIAL_BACKOFF_MS: u64 = 500;

/// Double the backoff, capped at `max_ms`.
fn next_backoff(current_ms: u64, max_ms: u64) -> u64 {
    current_ms.saturating_mul(2).min(max_ms)
}

/// Outer reconnect loop with exponential backoff; exits on `shutdown`.
pub async fn run(
    cfg: SessionConfig,
    services: Arc<ServiceRegistry>,
    executor: Arc<Executor>,
    status: SharedSessionStatus,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut backoff_ms = INITIAL_BACKOFF_MS;
    let max_ms = u64::try_from(cfg.max_backoff.as_millis()).unwrap_or(u64::MAX);

    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        {
            let mut s = status.write().await;
            s.state = ConnState::Reconnecting;
        }

        // `connect_and_serve` resets `backoff_ms` once the handshake completes.
        match connect_and_serve(
            &cfg,
            services.clone(),
            executor.clone(),
            status.clone(),
            &mut shutdown,
            &mut backoff_ms,
        )
        .await
        {
            Ok(reason) => {
                info!(reason = %reason, "session ended cleanly");
                if *shutdown.borrow() {
                    return Ok(());
                }
            }
            Err(e) => {
                warn!(error = %e, "session terminated; backing off");
            }
        }

        {
            let mut s = status.write().await;
            s.state = ConnState::Down;
        }

        if *shutdown.borrow() {
            return Ok(());
        }
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = sleep(Duration::from_millis(backoff_ms)) => {}
        }
        backoff_ms = next_backoff(backoff_ms, max_ms);
    }
}

/// One WS lifecycle: connect, hello+ack, then run reader/heartbeat/writer until end.
async fn connect_and_serve(
    cfg: &SessionConfig,
    services: Arc<ServiceRegistry>,
    executor: Arc<Executor>,
    status: SharedSessionStatus,
    shutdown: &mut watch::Receiver<bool>,
    backoff_ms: &mut u64,
) -> anyhow::Result<&'static str> {
    let mut request = cfg
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| anyhow::anyhow!("invalid wss url: {e}"))?;
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", cfg.token))
            .map_err(|e| anyhow::anyhow!("invalid token header: {e}"))?,
    );

    let (ws_stream, _http_resp) = connect_async(request).await?;
    info!("websocket connected; sending hello");

    let (mut sink, mut stream) = ws_stream.split();

    let hello = Frame::Hello(HelloFrame {
        protocol_versions_supported: Some(vec![PROTOCOL_VERSION]),
        protocol_version: None,
        proxy_build_version: Some(cfg.proxy_build_version.clone()),
    });
    sink.send(Message::Text(serde_json::to_string(&hello)?))
        .await?;

    // Wait for hello ack.
    let ack_msg = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .map_err(|_| anyhow::anyhow!("timeout awaiting hello ack"))?
        .ok_or_else(|| anyhow::anyhow!("stream ended before hello ack"))??;
    match parse_frame(&ack_msg)? {
        Frame::Hello(h) => match h.protocol_version {
            Some(v) if v == PROTOCOL_VERSION => {
                info!(version = v, "hello ack received");
            }
            Some(other) => {
                anyhow::bail!("serum acked unsupported protocol_version={other}");
            }
            None => anyhow::bail!("hello ack missing protocol_version"),
        },
        other => anyhow::bail!("expected Hello ack, got {:?}", frame_type(&other)),
    }

    {
        let mut s = status.write().await;
        s.state = ConnState::Connected;
        s.last_frame_at = Some(Instant::now());
    }
    // Handshake done: clear backoff so the next reconnect starts from the initial delay.
    *backoff_ms = INITIAL_BACKOFF_MS;

    let (tx, mut rx) = mpsc::channel::<Frame>(256);
    let requests_since_last = Arc::new(AtomicU64::new(0));
    let started = Instant::now();

    // Writer task: single owner of the WS sink.
    let writer_handle = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let text = match serde_json::to_string(&frame) {
                Ok(t) => t,
                Err(e) => {
                    error!(error = %e, "failed to serialize outbound frame");
                    continue;
                }
            };
            if let Err(e) = sink.send(Message::Text(text)).await {
                error!(error = %e, "failed to write to websocket; dropping writer");
                break;
            }
        }
        // Try a clean close before dropping the sink.
        let _ = sink
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "shutdown".into(),
            })))
            .await;
        let _ = sink.close().await;
    });

    // Heartbeat task.
    let hb_tx = tx.clone();
    let hb_counter = requests_since_last.clone();
    let hb_interval = cfg.heartbeat_interval;
    let hb_shutdown = shutdown.clone();
    let heartbeat_handle = tokio::spawn(async move {
        heartbeat_loop(hb_tx, hb_counter, hb_interval, started, hb_shutdown).await;
    });

    // Reader loop.
    let read_result = read_loop(
        &mut stream,
        tx.clone(),
        services,
        executor,
        status.clone(),
        requests_since_last.clone(),
        shutdown,
    )
    .await;

    // The writer exits once every `tx` clone is gone: ours (dropped here), the heartbeat's
    // (aborted here) and those held by in-flight executor tasks. The timeout bounds how long
    // we wait for in-flight responses to drain.
    drop(tx);
    heartbeat_handle.abort();

    // Wait for writer to drain or finish quickly.
    let _ = tokio::time::timeout(Duration::from_secs(10), writer_handle).await;

    read_result
}

async fn heartbeat_loop(
    tx: mpsc::Sender<Frame>,
    counter: Arc<AtomicU64>,
    interval: Duration,
    started: Instant,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await; // first tick fires immediately; skip it
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let uptime = started.elapsed().as_secs();
                let handled = counter.swap(0, Ordering::Relaxed);
                debug!(uptime_seconds = uptime, requests_handled_since_last = handled, "sending heartbeat");
                let frame = Frame::Heartbeat(HeartbeatFrame {
                    protocol_version: ProtocolV1,
                    uptime_seconds: uptime,
                    requests_handled_since_last: handled,
                });
                if tx.send(frame).await.is_err() {
                    debug!("heartbeat channel closed; exiting heartbeat loop");
                    return;
                }
            }
            _ = shutdown.changed() => {
                return;
            }
        }
    }
}

/// Read inbound WS messages; dispatch data frames to `handle_message` until shutdown or peer close.
async fn read_loop<S>(
    stream: &mut S,
    tx: mpsc::Sender<Frame>,
    services: Arc<ServiceRegistry>,
    executor: Arc<Executor>,
    status: SharedSessionStatus,
    req_counter: Arc<AtomicU64>,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<&'static str>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                info!("shutdown signalled; ending read loop");
                return Ok("shutdown");
            }
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => return Err(anyhow::anyhow!("websocket closed by peer")),
                };
                match msg {
                    Message::Text(_) | Message::Binary(_) => {
                        {
                            let mut s = status.write().await;
                            s.last_frame_at = Some(Instant::now());
                        }
                        handle_message(
                            &msg,
                            tx.clone(),
                            services.clone(),
                            executor.clone(),
                            req_counter.clone(),
                        )
                        .await;
                    }
                    Message::Ping(p) => {
                        // tokio-tungstenite auto-replies with Pong; we only observe.
                        debug!(bytes = p.len(), "ping received");
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => {
                        info!("close frame received");
                        return Ok("peer closed");
                    }
                    Message::Frame(_) => {}
                }
            }
        }
    }
}

/// What should happen to one inbound frame, decided by [`decide`] and carried
/// out by [`handle_message`]. Holds no channel or task; it is just the verdict.
pub enum Action {
    /// Valid request for a known service: spawn an executor task to forward it.
    Forward(RequestFrame, Arc<ResolvedService>),
    /// Per-request rejection (bad `request_id`, unknown service): one response frame.
    Reject(ResponseFrame),
    /// Out-of-band protocol error not tied to a request (parse failure).
    Emit(ErrorFrameBody),
    /// Non-actionable frame: heartbeat, stray mid-session hello, or a
    /// response/error that only flows proxy to serum.
    Ignore,
}

/// Decide what to do with one inbound frame. `parsed` is the parse result, so the
/// parse-failure path also routes through here (-> [`Action::Emit`]).
pub fn decide(parsed: Result<Frame, anyhow::Error>, services: &ServiceRegistry) -> Action {
    let frame = match parsed {
        Ok(f) => f,
        Err(e) => {
            warn!(error = %e, "failed to parse inbound frame");
            return Action::Emit(ErrorFrameBody {
                protocol_version: ProtocolV1,
                error: FrameError {
                    code: ErrorCode::FrameParseFailed,
                    message: format!("frame parse failed: {e}"),
                    retryable: false,
                },
            });
        }
    };

    match frame {
        Frame::Request(req) => {
            if !is_valid_request_id(&req.request_id) {
                warn!(request_id = %req.request_id, "invalid request_id; rejecting");
                return Action::Reject(ResponseFrame::error(
                    req.request_id.clone(),
                    FrameError {
                        code: ErrorCode::FrameParseFailed,
                        message: "request_id is not a UUIDv7 hex string".into(),
                        retryable: false,
                    },
                ));
            }
            match resolve_service(services, &req.target_service) {
                Ok(service) => Action::Forward(req, service),
                Err(err) => {
                    warn!(
                        request_id = %req.request_id,
                        target_service = %req.target_service,
                        "unknown target_service; rejecting"
                    );
                    Action::Reject(ResponseFrame::error(
                        req.request_id.clone(),
                        err.to_frame_error(),
                    ))
                }
            }
        }
        Frame::Heartbeat(_) => {
            debug!("heartbeat from peer");
            Action::Ignore
        }
        Frame::Hello(_) => {
            warn!("unexpected mid-session Hello frame");
            Action::Ignore
        }
        Frame::Response(_) | Frame::Error(_) => {
            // These frames only flow proxy to serum; receiving one is a peer bug.
            debug!("ignoring response/error frame from serum");
            Action::Ignore
        }
    }
}

/// Thin shell over [`decide`]: parse the frame, then carry out the verdict. The
/// request-counter increment and the executor task spawn live here, not in `decide`.
async fn handle_message(
    msg: &Message,
    tx: mpsc::Sender<Frame>,
    services: Arc<ServiceRegistry>,
    executor: Arc<Executor>,
    counter: Arc<AtomicU64>,
) {
    match decide(parse_frame(msg), &services) {
        Action::Forward(req, service) => {
            tokio::spawn(async move {
                let resp = executor.execute(req, &service).await;
                counter.fetch_add(1, Ordering::Relaxed);
                let _ = tx.send(Frame::Response(resp)).await;
            });
        }
        Action::Reject(resp) => {
            let _ = tx.send(Frame::Response(resp)).await;
        }
        Action::Emit(err) => {
            let _ = tx.send(Frame::Error(err)).await;
        }
        Action::Ignore => {}
    }
}

/// Resolve `target_service` against the registry, or the typed error for an
/// unknown one (`404_UNKNOWN_SERVICE`). Internal helper of [`decide`].
fn resolve_service(
    services: &ServiceRegistry,
    target_service: &str,
) -> Result<Arc<ResolvedService>, ProxyError> {
    services
        .get(target_service)
        .cloned()
        .ok_or_else(|| ProxyError::UnknownService {
            service: target_service.to_string(),
        })
}

fn parse_frame(msg: &Message) -> Result<Frame, anyhow::Error> {
    let text = match msg {
        Message::Text(t) => t.as_str(),
        Message::Binary(b) => {
            std::str::from_utf8(b).map_err(|e| anyhow::anyhow!("binary frame is not utf-8: {e}"))?
        }
        _ => anyhow::bail!("non-data websocket message: {:?}", msg),
    };
    let frame: Frame = serde_json::from_str(text)?;
    Ok(frame)
}

fn frame_type(f: &Frame) -> &'static str {
    match f {
        Frame::Hello(_) => "hello",
        Frame::Heartbeat(_) => "heartbeat",
        Frame::Request(_) => "request",
        Frame::Response(_) => "response",
        Frame::Error(_) => "error",
    }
}

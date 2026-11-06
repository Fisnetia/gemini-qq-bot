//! OneBot v11 forward WebSocket client.
//!
//! The bot connects *out* to SnowLuma (forward WS), authenticates with an
//! `Authorization: Bearer` header when a token is configured, sends heartbeat
//! pings so a half-open socket is detected, and reconnects with the backoff the
//! design specifies (1, 2, 5, 10, 30, 60 seconds + jitter, then periodic retry at
//! 60 seconds forever).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::onebot::action::SendOutcome;
use crate::onebot::event::{FrameKind, IncomingFrame, RawActionResponse};

/// Backoff schedule from the design, in seconds.
pub const BACKOFF_SECONDS: [u64; 6] = [1, 2, 5, 10, 30, 60];

/// Health of the OneBot link, surfaced on the admin page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionStatus {
    pub connected: bool,
    /// Monotonic connection generation; it resets the echo space.
    pub generation: u64,
    pub last_frame_at: Option<i64>,
    pub last_error: Option<String>,
    pub reconnect_attempt: u32,
}

/// A pending action awaiting its response.
struct Pending {
    echo: String,
    tx: oneshot::Sender<Value>,
}

/// Shared handle used by the pipeline to send actions.
#[derive(Clone)]
pub struct ConnectionHandle {
    outbound: mpsc::Sender<Value>,
    pending: Arc<Mutex<Vec<Pending>>>,
    status: Arc<RwLock<ConnectionStatus>>,
    counter: Arc<AtomicU64>,
}

impl std::fmt::Debug for ConnectionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionHandle").finish_non_exhaustive()
    }
}

impl ConnectionHandle {
    /// Connection state snapshot.
    pub async fn status(&self) -> ConnectionStatus {
        self.status.read().await.clone()
    }

    /// True when the socket is believed to be up.
    pub async fn is_connected(&self) -> bool {
        self.status.read().await.connected
    }

    /// Perform one action and wait for its echo-matched response.
    ///
    /// Returns `SendOutcome::Unknown` when the socket is down or the response does
    /// not arrive within `timeout`, because delivery cannot be ruled out.
    pub async fn call(&self, action: &str, params: Value, timeout: Duration) -> SendOutcome {
        match self.call_raw(action, params, timeout).await {
            Ok(resp) => crate::onebot::action::classify_send_response(&resp),
            Err(reason) => SendOutcome::Unknown { reason },
        }
    }

    /// Perform one read-only action and return its `data` payload.
    ///
    /// For query actions (`get_group_info` and friends) whose result the
    /// caller actually needs: [`Self::call`] classifies for send
    /// semantics and drops the payload.
    pub async fn call_data(
        &self,
        action: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let resp = self.call_raw(action, params, timeout).await?;
        let retcode = resp.retcode.unwrap_or(-1);
        if retcode != 0 {
            return Err(format!("action {action} rejected: retcode {retcode}"));
        }
        Ok(resp.data.unwrap_or(Value::Null))
    }

    /// The shared echo-matched request round trip behind `call` and
    /// `call_data`. An `Err` reason means delivery of the *response*
    /// cannot be ruled in or out, exactly like `SendOutcome::Unknown`.
    async fn call_raw(
        &self,
        action: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<RawActionResponse, String> {
        if !self.is_connected().await {
            return Err("onebot socket not connected".into());
        }
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let echo = format!("e{n}");
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.push(Pending {
            echo: echo.clone(),
            tx,
        });

        let frame = json!({ "action": action, "params": params, "echo": echo });
        if self.outbound.send(frame).await.is_err() {
            self.drop_pending(&echo).await;
            return Err("outbound channel closed".into());
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(value)) => serde_json::from_value(value)
                .map_err(|e| format!("unparsable action response: {e}")),
            Ok(Err(_)) => Err("response channel dropped".into()),
            Err(_) => {
                // Timed out: the action may or may not have been executed.
                self.drop_pending(&echo).await;
                Err("action response timeout".into())
            }
        }
    }

    /// Send a raw action without waiting for the response. Used for heartbeats.
    pub async fn fire(&self, action: &str, params: Value) {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let frame = json!({ "action": action, "params": params, "echo": format!("hb{n}") });
        let _ = self.outbound.send(frame).await;
    }

    async fn drop_pending(&self, echo: &str) {
        let mut guard = self.pending.lock().await;
        guard.retain(|p| p.echo != echo);
    }
}

/// Supervisor that owns the socket and reconnects forever.
pub struct Connection {
    url: String,
    access_token: Option<String>,
    handle: ConnectionHandle,
    outbound_rx: mpsc::Receiver<Value>,
    heartbeat: Duration,
}

/// Event sink the supervisor forwards normalized-capable frames into.
#[async_trait::async_trait]
pub trait FrameSink: Send + Sync + 'static {
    /// Handle one inbound event frame.
    async fn on_event(&self, value: Value);
    /// Called whenever the socket (re)connects, so the caller can resync.
    async fn on_connected(&self) {}
}

impl Connection {
    /// Create the supervisor and the shared handle.
    pub fn new(url: impl Into<String>, access_token: Option<String>) -> (Self, ConnectionHandle) {
        let (tx, rx) = mpsc::channel(256);
        let handle = ConnectionHandle {
            outbound: tx,
            pending: Arc::new(Mutex::new(Vec::new())),
            status: Arc::new(RwLock::new(ConnectionStatus::default())),
            counter: Arc::new(AtomicU64::new(1)),
        };
        (
            Self {
                url: url.into(),
                access_token,
                handle: handle.clone(),
                outbound_rx: rx,
                heartbeat: Duration::from_secs(30),
            },
            handle,
        )
    }

    /// Override the heartbeat interval (tests use a short one).
    pub fn with_heartbeat(mut self, heartbeat: Duration) -> Self {
        self.heartbeat = heartbeat;
        self
    }

    /// Run the supervisor until the process exits.
    pub async fn run<S: FrameSink>(mut self, sink: Arc<S>) {
        let mut attempt: usize = 0;
        loop {
            let delay = if attempt == 0 {
                Duration::ZERO
            } else {
                let base = BACKOFF_SECONDS[(attempt - 1).min(BACKOFF_SECONDS.len() - 1)];
                // Deterministic jitter derived from the attempt number keeps tests
                // reproducible while avoiding thundering herds in production.
                let jitter_ms = (attempt as u64 * 137) % 500;
                Duration::from_millis(base * 1000 + jitter_ms)
            };
            if !delay.is_zero() {
                {
                    let mut st = self.handle.status.write().await;
                    st.reconnect_attempt = attempt as u32;
                }
                tokio::time::sleep(delay).await;
            }

            match self.connect_once(&sink).await {
                Ok(()) => {
                    attempt = 0;
                }
                Err(reason) => {
                    attempt = attempt.saturating_add(1);
                    let mut st = self.handle.status.write().await;
                    st.connected = false;
                    st.last_error = Some(reason);
                }
            }
            // Once we have backed off to 60s we keep retrying at that cadence.
            if attempt > BACKOFF_SECONDS.len() {
                attempt = BACKOFF_SECONDS.len();
            }
        }
    }

    async fn connect_once<S: FrameSink>(&mut self, sink: &Arc<S>) -> Result<(), String> {
        let mut request = self
            .url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("非法 WebSocket URL: {e}"))?;
        if let Some(token) = &self.access_token {
            let value = format!("Bearer {token}");
            request.headers_mut().insert(
                "Authorization",
                value
                    .parse()
                    .map_err(|_| "无法构造 Authorization 头".to_string())?,
            );
        }

        let (stream, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| format!("连接 OneBot 失败: {e}"))?;
        let (mut write, mut read) = stream.split();

        {
            let mut st = self.handle.status.write().await;
            st.connected = true;
            st.generation += 1;
            st.last_error = None;
            st.last_frame_at = Some(crate::now_unix());
        }
        sink.on_connected().await;

        let mut heartbeat = tokio::time::interval(self.heartbeat);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                biased;
                outgoing = self.outbound_rx.recv() => {
                    let Some(frame) = outgoing else {
                        return Err("outbound channel closed".into());
                    };
                    let text = serde_json::to_string(&frame).map_err(|e| e.to_string())?;
                    if let Err(e) = write.send(Message::Text(text.into())).await {
                        return Err(format!("写入失败: {e}"));
                    }
                }
                _ = heartbeat.tick() => {
                    let frame = json!({
                        "action": "get_status",
                        "params": {},
                        "echo": format!("hb{}", crate::now_unix())
                    });
                    if let Ok(text) = serde_json::to_string(&frame)
                        && let Err(e) = write.send(Message::Text(text.into())).await
                    {
                        return Err(format!("心跳写入失败: {e}"));
                    }
                }
                inbound = read.next() => {
                    match inbound {
                        None => return Err("OneBot 连接被对端关闭".into()),
                        Some(Err(e)) => return Err(format!("读取失败: {e}")),
                        Some(Ok(Message::Text(text))) => {
                            self.route(&text, sink).await;
                        }
                        Some(Ok(Message::Binary(_))) => {
                            // Binary frames are not part of the OneBot v11 contract.
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = write.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(Message::Close(frame)) ) => {
                            return Err(match frame {
                                Some(f) => format!("对端关闭连接: {}", f.reason),
                                None => "对端关闭连接".into(),
                            });
                        }
                        Some(Ok(_)) => {}
                    }
                }
            }
        }
    }

    async fn route<S: FrameSink>(&self, text: &str, sink: &Arc<S>) {
        let value: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };
        {
            let mut st = self.handle.status.write().await;
            st.last_frame_at = Some(crate::now_unix());
        }
        match IncomingFrame::classify(&value) {
            FrameKind::Response => {
                let echo = value
                    .get("echo")
                    .and_then(|e| e.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut guard = self.handle.pending.lock().await;
                if let Some(pos) = guard.iter().position(|p| p.echo == echo) {
                    let pending = guard.remove(pos);
                    let _ = pending.tx.send(value);
                }
            }
            FrameKind::Event => sink.on_event(value).await,
            FrameKind::Unknown => {}
        }
    }
}

/// Classify without parsing twice; exported for tests.
pub fn classify_frame(value: &Value) -> FrameKind {
    IncomingFrame::classify(value)
}

// Silence an unused-import warning when the trait object form is not used.
#[allow(dead_code)]
fn _assert_parts(_: SendOutcome) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_matches_design() {
        assert_eq!(BACKOFF_SECONDS, [1, 2, 5, 10, 30, 60]);
    }

    #[test]
    fn backoff_is_capped_at_last_entry() {
        // Indexing must clamp instead of panicking when attempts exceed the table.
        let idx = (7usize - 1).min(BACKOFF_SECONDS.len() - 1);
        assert_eq!(BACKOFF_SECONDS[idx], 60);
    }

    #[test]
    fn default_status_is_disconnected() {
        let st = ConnectionStatus::default();
        assert!(!st.connected);
        assert_eq!(st.generation, 0);
        assert!(st.last_error.is_none());
    }

    #[tokio::test]
    async fn call_on_disconnected_socket_is_unknown_not_error() {
        let (_conn, handle) = Connection::new("ws://127.0.0.1:1", None);
        let out = handle
            .call("send_group_msg", json!({}), Duration::from_millis(50))
            .await;
        assert!(
            matches!(out, SendOutcome::Unknown { .. }),
            "未连接时必须报告 unknown 而不是成功"
        );
    }

    #[test]
    fn classifies_frames_consistently() {
        assert_eq!(
            classify_frame(&json!({"post_type":"message"})),
            FrameKind::Event
        );
        assert_eq!(
            classify_frame(&json!({"retcode":0,"echo":"x","status":"ok"})),
            FrameKind::Response
        );
    }
}

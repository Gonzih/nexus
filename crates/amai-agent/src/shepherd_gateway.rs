use std::sync::Arc;
use std::time::Duration;

use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use soul_core::error::{SoulError, SoulResult};
use soul_core::gateway::{GatewayEvent, GatewayMessage};
use soul_core::types::AgentEvent;

// ─── Protocol Envelope ──────────────────────────────────────────────────────

const PROTOCOL_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u8,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub session_id: String,
    pub ts: String,
    pub payload: serde_json::Value,
}

impl Envelope {
    pub fn new(msg_type: &str, session_id: &str, payload: serde_json::Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            msg_type: msg_type.to_string(),
            session_id: session_id.to_string(),
            ts: chrono::Utc::now().to_rfc3339(),
            payload,
        }
    }
}

// ─── ShepherdGateway ────────────────────────────────────────────────────────

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMessage>;
type WsStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

pub struct ShepherdGateway {
    url: String,
    session_id: String,
    cwd: String,
    max_turns: usize,
    heartbeat_secs: u64,
    identity_id: Option<String>,
    identity_kid: Option<String>,
    identity_signature: Option<String>,
    shutdown_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    ws_sink: Arc<Mutex<Option<WsSink>>>,
}

impl ShepherdGateway {
    pub fn new(url: &str, cwd: &str, max_turns: usize) -> Self {
        // Extract session_id from URL: ws://host:port/ws/sessions/{session_id}
        let session_id = url
            .rsplit('/')
            .next()
            .unwrap_or("unknown")
            .to_string();

        Self {
            url: url.to_string(),
            session_id,
            cwd: cwd.to_string(),
            max_turns,
            heartbeat_secs: 15,
            identity_id: None,
            identity_kid: None,
            identity_signature: None,
            shutdown_tx: Arc::new(Mutex::new(None)),
            ws_sink: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_heartbeat_secs(mut self, secs: u64) -> Self {
        self.heartbeat_secs = secs;
        self
    }

    pub fn with_identity(mut self, identity: Option<&super::identity::AgentIdentity>) -> Self {
        if let Some(id) = identity {
            self.identity_id = Some(id.identity_id.clone());
            self.identity_kid = Some(id.kid.clone());
            // Sign the session_id for verification
            self.identity_signature = Some(id.sign(self.session_id.as_bytes()));
        }
        self
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get a shared reference to the WS sink for event forwarding.
    pub fn sink(&self) -> Arc<Mutex<Option<WsSink>>> {
        self.ws_sink.clone()
    }

    async fn send_envelope(
        sink: &Arc<Mutex<Option<WsSink>>>,
        envelope: &Envelope,
    ) -> SoulResult<()> {
        let json = serde_json::to_string(envelope)
            .map_err(|e| SoulError::Provider(format!("Serialize error: {e}")))?;
        let mut guard = sink.lock().await;
        if let Some(ref mut ws) = *guard {
            ws.send(WsMessage::Text(json.into()))
                .await
                .map_err(|e| SoulError::Provider(format!("WS send error: {e}")))?;
        }
        Ok(())
    }

    /// Connect WS, register with shepherd, start read + heartbeat loops.
    pub async fn start(
        &self,
        event_tx: mpsc::UnboundedSender<GatewayEvent>,
    ) -> SoulResult<()> {
        let (ws_stream, _response) = connect_async(&self.url)
            .await
            .map_err(|e| SoulError::Provider(format!("WS connect failed: {e}")))?;

        let (sink, stream) = ws_stream.split();
        {
            let mut guard = self.ws_sink.lock().await;
            *guard = Some(sink);
        }

        // Send agent.register (includes identity if available)
        let register = Envelope::new(
            "agent.register",
            &self.session_id,
            serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "cwd": self.cwd,
                "max_turns": self.max_turns,
                "identity_id": self.identity_id,
                "identity_kid": self.identity_kid,
                "identity_signature": self.identity_signature,
            }),
        );
        Self::send_envelope(&self.ws_sink, &register).await?;

        // Shutdown channel
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        {
            let mut guard = self.shutdown_tx.lock().await;
            *guard = Some(shutdown_tx);
        }

        // Spawn read loop
        let ws_sink = self.ws_sink.clone();
        let session_id = self.session_id.clone();
        tokio::spawn(Self::read_loop(stream, event_tx, ws_sink, session_id));

        // Spawn heartbeat loop
        let heartbeat_sink = self.ws_sink.clone();
        let heartbeat_session = self.session_id.clone();
        let heartbeat_interval = self.heartbeat_secs;
        tokio::spawn(Self::heartbeat_loop(
            heartbeat_sink,
            heartbeat_session,
            heartbeat_interval,
            shutdown_rx,
        ));

        tracing::info!(url = %self.url, session_id = %self.session_id, "Shepherd gateway connected");
        Ok(())
    }

    async fn read_loop(
        mut stream: WsStream,
        event_tx: mpsc::UnboundedSender<GatewayEvent>,
        _ws_sink: Arc<Mutex<Option<WsSink>>>,
        session_id: String,
    ) {
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(WsMessage::Text(text)) => {
                    match serde_json::from_str::<Envelope>(&text) {
                        Ok(envelope) => {
                            Self::handle_downstream(envelope, &event_tx, &session_id);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Invalid envelope from shepherd");
                        }
                    }
                }
                Ok(WsMessage::Close(_)) => {
                    tracing::info!("Shepherd closed WS connection");
                    break;
                }
                Ok(WsMessage::Ping(data)) => {
                    // Tungstenite auto-responds to pings, but log it
                    tracing::trace!(len = data.len(), "WS ping received");
                }
                Err(e) => {
                    tracing::error!(error = %e, "WS read error");
                    break;
                }
                _ => {}
            }
        }

        // Channel closed — signal gateway shutdown
        let _ = event_tx.send(GatewayEvent::Error {
            source: "shepherd".into(),
            message: "WebSocket connection closed".into(),
        });
    }

    fn handle_downstream(
        envelope: Envelope,
        event_tx: &mpsc::UnboundedSender<GatewayEvent>,
        session_id: &str,
    ) {
        match envelope.msg_type.as_str() {
            "shepherd.ack" => {
                tracing::info!(session_id, "Shepherd acknowledged registration");
            }
            "shepherd.task" => {
                let task = envelope.payload.get("task")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let _ = event_tx.send(GatewayEvent::MessageReceived {
                    channel_id: session_id.to_string(),
                    sender: "shepherd".into(),
                    text: task,
                    metadata: serde_json::json!({
                        "max_turns": envelope.payload.get("max_turns"),
                    }),
                });
            }
            "shepherd.steer" => {
                let message = envelope.payload.get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let _ = event_tx.send(GatewayEvent::MessageReceived {
                    channel_id: session_id.to_string(),
                    sender: "shepherd".into(),
                    text: message,
                    metadata: serde_json::json!({ "steer": true }),
                });
            }
            "shepherd.stop" => {
                let reason = envelope.payload.get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("requested")
                    .to_string();
                tracing::info!(reason = %reason, "Shepherd requested stop");
                let _ = event_tx.send(GatewayEvent::Error {
                    source: "shepherd".into(),
                    message: format!("Stop requested: {reason}"),
                });
            }
            other => {
                tracing::debug!(msg_type = other, "Unknown message type from shepherd");
            }
        }
    }

    async fn heartbeat_loop(
        ws_sink: Arc<Mutex<Option<WsSink>>>,
        session_id: String,
        interval_secs: u64,
        mut shutdown_rx: oneshot::Receiver<()>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        interval.tick().await; // skip first immediate tick

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let heartbeat = Envelope::new("agent.heartbeat", &session_id, serde_json::json!({}));
                    if let Err(e) = Self::send_envelope(&ws_sink, &heartbeat).await {
                        tracing::warn!(error = %e, "Failed to send heartbeat");
                        break;
                    }
                    tracing::trace!("Heartbeat sent");
                }
                _ = &mut shutdown_rx => {
                    tracing::debug!("Heartbeat loop shutting down");
                    break;
                }
            }
        }
    }

    pub async fn send_result(&self, text: &str, turns_used: usize) -> SoulResult<()> {
        let envelope = Envelope::new(
            "agent.result",
            &self.session_id,
            serde_json::json!({
                "text": text,
                "turns_used": turns_used,
            }),
        );
        Self::send_envelope(&self.ws_sink, &envelope).await
    }

    pub async fn stop(&self) -> SoulResult<()> {
        // Signal heartbeat loop to stop
        let mut guard = self.shutdown_tx.lock().await;
        if let Some(tx) = guard.take() {
            let _ = tx.send(());
        }

        // Close WS connection
        let mut sink_guard = self.ws_sink.lock().await;
        if let Some(ref mut ws) = *sink_guard {
            let _ = ws.send(WsMessage::Close(None)).await;
        }
        *sink_guard = None;

        Ok(())
    }
}

// Gateway trait implementation — allows ShepherdGateway to plug into the
// same message loop used by TelegramGateway.
#[async_trait::async_trait]
impl soul_core::gateway::Gateway for ShepherdGateway {
    fn name(&self) -> &str {
        "shepherd"
    }

    async fn start(&self, event_tx: mpsc::UnboundedSender<GatewayEvent>) -> SoulResult<()> {
        self.start(event_tx).await
    }

    async fn send(&self, _channel_id: &str, message: GatewayMessage) -> SoulResult<()> {
        match message {
            GatewayMessage::Text { text } => {
                self.send_result(&text, 0).await
            }
        }
    }

    async fn stop(&self) -> SoulResult<()> {
        self.stop().await
    }
}

// ─── ShepherdEventForwarder ─────────────────────────────────────────────────

/// Forwards AgentEvents from the agent loop to shepherd over WebSocket.
/// Also logs locally via DiskLogger for dual-path persistence.
pub struct ShepherdEventForwarder {
    ws_sink: Arc<Mutex<Option<WsSink>>>,
    session_id: String,
    batch_interval_ms: u64,
}

impl ShepherdEventForwarder {
    pub fn new(ws_sink: Arc<Mutex<Option<WsSink>>>, session_id: &str) -> Self {
        Self {
            ws_sink,
            session_id: session_id.to_string(),
            batch_interval_ms: 50,
        }
    }

    /// Spawn a task that reads events and forwards them over WS + soullog.
    /// Returns a handle that completes when the event channel closes.
    pub fn spawn(
        self,
        mut event_rx: mpsc::UnboundedReceiver<AgentEvent>,
        local_logger: Option<Arc<super::DiskLogger>>,
        soullog: Option<Arc<super::soullog_client::SoullogClient>>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut batch: Vec<AgentEvent> = Vec::new();
            let flush_interval = Duration::from_millis(self.batch_interval_ms);

            loop {
                // Collect events with a small batch window for MessageDelta
                tokio::select! {
                    event = event_rx.recv() => {
                        match event {
                            Some(evt) => {
                                // Log locally
                                if let Some(ref logger) = local_logger {
                                    logger.log_event(&evt);
                                }

                                // Log to soullog (centralized)
                                if let Some(ref sl) = soullog {
                                    sl.log_event(&evt);
                                }

                                let is_delta = matches!(&evt, AgentEvent::MessageDelta { .. });
                                batch.push(evt);

                                // Flush immediately for non-delta events
                                if !is_delta {
                                    self.flush_batch(&mut batch).await;
                                }
                            }
                            None => {
                                // Channel closed — flush remaining and exit
                                if !batch.is_empty() {
                                    self.flush_batch(&mut batch).await;
                                }
                                break;
                            }
                        }
                    }
                    _ = tokio::time::sleep(flush_interval), if !batch.is_empty() => {
                        self.flush_batch(&mut batch).await;
                    }
                }
            }
        })
    }

    async fn flush_batch(&self, batch: &mut Vec<AgentEvent>) {
        for event in batch.drain(..) {
            let envelope = Envelope::new(
                "agent.event",
                &self.session_id,
                serde_json::to_value(&event).unwrap_or_default(),
            );
            if let Err(e) = ShepherdGateway::send_envelope(&self.ws_sink, &envelope).await {
                tracing::warn!(error = %e, "Failed to forward event to shepherd");
            }
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_serialization_roundtrip() {
        let envelope = Envelope::new(
            "agent.register",
            "sess_abc123",
            serde_json::json!({ "version": "0.1.0", "cwd": "/tmp", "max_turns": 50 }),
        );
        let json = serde_json::to_string(&envelope).unwrap();
        assert!(json.contains(r#""type":"agent.register""#));
        assert!(json.contains(r#""session_id":"sess_abc123""#));
        assert!(json.contains(r#""v":1"#));

        let parsed: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.msg_type, "agent.register");
        assert_eq!(parsed.session_id, "sess_abc123");
        assert_eq!(parsed.v, 1);
    }

    #[test]
    fn envelope_agent_event() {
        let event = AgentEvent::TurnStart { turn: 1 };
        let payload = serde_json::to_value(&event).unwrap();
        let envelope = Envelope::new("agent.event", "sess_test", payload);
        let json = serde_json::to_string(&envelope).unwrap();
        assert!(json.contains(r#""type":"agent.event""#));
        assert!(json.contains(r#""turn":1"#));
    }

    #[test]
    fn envelope_agent_result() {
        let envelope = Envelope::new(
            "agent.result",
            "sess_test",
            serde_json::json!({ "text": "done", "turns_used": 5 }),
        );
        let json = serde_json::to_string(&envelope).unwrap();
        assert!(json.contains(r#""type":"agent.result""#));
        assert!(json.contains(r#""turns_used":5"#));
    }

    #[test]
    fn envelope_heartbeat() {
        let envelope = Envelope::new("agent.heartbeat", "sess_hb", serde_json::json!({}));
        let json = serde_json::to_string(&envelope).unwrap();
        assert!(json.contains(r#""type":"agent.heartbeat""#));
    }

    #[test]
    fn shepherd_gateway_name() {
        let gw = ShepherdGateway::new(
            "ws://localhost:8084/ws/sessions/sess_abc",
            "/tmp",
            50,
        );
        assert_eq!(soul_core::gateway::Gateway::name(&gw), "shepherd");
    }

    #[test]
    fn session_id_extracted_from_url() {
        let gw = ShepherdGateway::new(
            "ws://localhost:8084/ws/sessions/sess_my_session",
            "/tmp",
            100,
        );
        assert_eq!(gw.session_id(), "sess_my_session");
    }

    #[test]
    fn shepherd_task_envelope_parsing() {
        let json = r#"{
            "v": 1,
            "type": "shepherd.task",
            "session_id": "sess_abc",
            "ts": "2026-02-16T00:00:00Z",
            "payload": { "task": "list all files", "max_turns": 10 }
        }"#;
        let envelope: Envelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.msg_type, "shepherd.task");
        assert_eq!(
            envelope.payload.get("task").unwrap().as_str().unwrap(),
            "list all files"
        );
    }

    #[test]
    fn shepherd_steer_envelope_parsing() {
        let json = r#"{
            "v": 1,
            "type": "shepherd.steer",
            "session_id": "sess_abc",
            "ts": "2026-02-16T00:00:00Z",
            "payload": { "message": "focus on the main module" }
        }"#;
        let envelope: Envelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.msg_type, "shepherd.steer");
    }

    #[test]
    fn shepherd_stop_envelope_parsing() {
        let json = r#"{
            "v": 1,
            "type": "shepherd.stop",
            "session_id": "sess_abc",
            "ts": "2026-02-16T00:00:00Z",
            "payload": { "reason": "budget exceeded" }
        }"#;
        let envelope: Envelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.msg_type, "shepherd.stop");
        assert_eq!(
            envelope.payload.get("reason").unwrap().as_str().unwrap(),
            "budget exceeded"
        );
    }
}

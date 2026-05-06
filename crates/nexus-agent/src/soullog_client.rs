use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc;

use soul_core::types::AgentEvent;

// ─── Soullog Event Schema ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SoullogEvent {
    pub id: String,
    pub service: String,
    pub topic: String,
    pub event_type: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub payload: serde_json::Value,
    pub metadata: SoullogMetadata,
}

#[derive(Debug, Clone, Serialize)]
pub struct SoullogMetadata {
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

// ─── Soullog Client ──────────────────────────────────────────────────────────

pub struct SoullogClient {
    url: String,
    service: String,
    identity_id: Option<String>,
    session_id: Option<String>,
    batch_tx: mpsc::UnboundedSender<SoullogEvent>,
    _flush_handle: tokio::task::JoinHandle<()>,
}

impl SoullogClient {
    pub fn new(
        url: &str,
        service: &str,
        identity_id: Option<String>,
        session_id: Option<String>,
        batch_size: usize,
        flush_interval_ms: u64,
    ) -> Self {
        let (batch_tx, batch_rx) = mpsc::unbounded_channel();
        let flush_url = url.trim_end_matches('/').to_string();

        let flush_handle = tokio::spawn(Self::flush_loop(
            batch_rx,
            flush_url,
            batch_size,
            flush_interval_ms,
        ));

        Self {
            url: url.trim_end_matches('/').to_string(),
            service: service.to_string(),
            identity_id,
            session_id,
            batch_tx,
            _flush_handle: flush_handle,
        }
    }

    /// Convert an AgentEvent to a SoullogEvent and enqueue for batch sending.
    pub fn log_event(&self, event: &AgentEvent) {
        let event_type = agent_event_type(event);
        let topic = agent_event_topic(event);

        let soullog_event = SoullogEvent {
            id: uuid::Uuid::new_v4().to_string(),
            service: self.service.clone(),
            topic,
            event_type,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            identity_id: self.identity_id.clone(),
            session_id: self.session_id.clone(),
            payload: serde_json::to_value(event).unwrap_or_default(),
            metadata: SoullogMetadata {
                version: "1.0".into(),
                correlation_id: self.session_id.clone(),
            },
        };

        let _ = self.batch_tx.send(soullog_event);
    }

    /// Send a custom event (not derived from AgentEvent).
    pub fn log_custom(&self, topic: &str, event_type: &str, payload: serde_json::Value) {
        let soullog_event = SoullogEvent {
            id: uuid::Uuid::new_v4().to_string(),
            service: self.service.clone(),
            topic: topic.to_string(),
            event_type: event_type.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            identity_id: self.identity_id.clone(),
            session_id: self.session_id.clone(),
            payload,
            metadata: SoullogMetadata {
                version: "1.0".into(),
                correlation_id: self.session_id.clone(),
            },
        };

        let _ = self.batch_tx.send(soullog_event);
    }

    async fn flush_loop(
        mut rx: mpsc::UnboundedReceiver<SoullogEvent>,
        url: String,
        batch_size: usize,
        flush_interval_ms: u64,
    ) {
        let client = reqwest::Client::new();
        let mut buffer: Vec<SoullogEvent> = Vec::with_capacity(batch_size);
        let flush_interval = Duration::from_millis(flush_interval_ms);

        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Some(evt) => {
                            buffer.push(evt);
                            if buffer.len() >= batch_size {
                                Self::flush_batch(&client, &url, &mut buffer).await;
                            }
                        }
                        None => {
                            // Channel closed — flush remaining and exit
                            if !buffer.is_empty() {
                                Self::flush_batch(&client, &url, &mut buffer).await;
                            }
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(flush_interval), if !buffer.is_empty() => {
                    Self::flush_batch(&client, &url, &mut buffer).await;
                }
            }
        }
    }

    async fn flush_batch(
        client: &reqwest::Client,
        url: &str,
        buffer: &mut Vec<SoullogEvent>,
    ) {
        if buffer.is_empty() {
            return;
        }

        let events: Vec<SoullogEvent> = buffer.drain(..).collect();
        let count = events.len();
        let endpoint = format!("{url}/ingest/batch");
        let body = serde_json::json!({ "events": events });

        match client.post(&endpoint).json(&body).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    tracing::debug!(count, "Flushed events to soullog");
                } else {
                    tracing::warn!(
                        status = %resp.status(),
                        count,
                        "Soullog batch ingest returned error"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, count, "Failed to send events to soullog");
            }
        }
    }
}

// ─── Event Type Mapping ──────────────────────────────────────────────────────

fn agent_event_type(event: &AgentEvent) -> String {
    match event {
        AgentEvent::AgentStart { .. } => "AgentStart".into(),
        AgentEvent::AgentEnd { .. } => "AgentEnd".into(),
        AgentEvent::TurnStart { .. } => "TurnStart".into(),
        AgentEvent::TurnEnd { .. } => "TurnEnd".into(),
        AgentEvent::MessageStart { .. } => "MessageStart".into(),
        AgentEvent::MessageDelta { .. } => "MessageDelta".into(),
        AgentEvent::MessageEnd { .. } => "MessageEnd".into(),
        AgentEvent::ToolExecutionStart { .. } => "ToolExecutionStart".into(),
        AgentEvent::ToolExecutionUpdate { .. } => "ToolExecutionUpdate".into(),
        AgentEvent::ToolExecutionEnd { .. } => "ToolExecutionEnd".into(),
        AgentEvent::CompactionStart { .. } => "CompactionStart".into(),
        AgentEvent::CompactionEnd { .. } => "CompactionEnd".into(),
        AgentEvent::StructuralRetry { .. } => "StructuralRetry".into(),
        AgentEvent::Error { .. } => "Error".into(),
        AgentEvent::Cost(_) => "Cost".into(),
        AgentEvent::PermissionCheck { .. } => "PermissionCheck".into(),
        AgentEvent::McpServerConnected { .. } => "McpServerConnected".into(),
        AgentEvent::SkillLoaded { .. } => "SkillLoaded".into(),
        AgentEvent::SubagentStart { .. } => "SubagentStart".into(),
        AgentEvent::SubagentEnd { .. } => "SubagentEnd".into(),
        AgentEvent::PlanExecutionStart { .. } => "PlanExecutionStart".into(),
        AgentEvent::PlanTaskStart { .. } => "PlanTaskStart".into(),
        AgentEvent::PlanTaskEnd { .. } => "PlanTaskEnd".into(),
        AgentEvent::PlanExecutionEnd { .. } => "PlanExecutionEnd".into(),
    }
}

fn agent_event_topic(event: &AgentEvent) -> String {
    match event {
        AgentEvent::Error { .. } | AgentEvent::StructuralRetry { .. } => {
            "soullog.agent.errors".into()
        }
        AgentEvent::ToolExecutionStart { .. }
        | AgentEvent::ToolExecutionUpdate { .. }
        | AgentEvent::ToolExecutionEnd { .. } => "soullog.agent.tools".into(),
        AgentEvent::SubagentStart { .. } | AgentEvent::SubagentEnd { .. } => {
            "soullog.agent.subagents".into()
        }
        AgentEvent::PlanExecutionStart { .. }
        | AgentEvent::PlanTaskStart { .. }
        | AgentEvent::PlanTaskEnd { .. }
        | AgentEvent::PlanExecutionEnd { .. } => "soullog.agent.plans".into(),
        AgentEvent::Cost(_) => "soullog.agent.cost".into(),
        _ => "soullog.agent.events".into(),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_type_mapping() {
        assert_eq!(
            agent_event_type(&AgentEvent::TurnStart { turn: 1 }),
            "TurnStart"
        );
        assert_eq!(
            agent_event_type(&AgentEvent::Error {
                message: "test".into()
            }),
            "Error"
        );
    }

    #[test]
    fn event_topic_mapping() {
        assert_eq!(
            agent_event_topic(&AgentEvent::TurnStart { turn: 1 }),
            "soullog.agent.events"
        );
        assert_eq!(
            agent_event_topic(&AgentEvent::Error {
                message: "test".into()
            }),
            "soullog.agent.errors"
        );
        assert_eq!(
            agent_event_topic(&AgentEvent::ToolExecutionStart {
                tool_call_id: "1".into(),
                tool_name: "read".into(),
                arguments: serde_json::json!({}),
            }),
            "soullog.agent.tools"
        );
    }

    #[test]
    fn soullog_event_serialization() {
        let event = SoullogEvent {
            id: "test_id".into(),
            service: "nexus-agent".into(),
            topic: "soullog.agent.events".into(),
            event_type: "TurnStart".into(),
            timestamp: "2026-02-16T00:00:00Z".into(),
            identity_id: Some("id_123".into()),
            session_id: Some("sess_abc".into()),
            payload: serde_json::json!({"turn": 1}),
            metadata: SoullogMetadata {
                version: "1.0".into(),
                correlation_id: Some("sess_abc".into()),
            },
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"eventType\":\"TurnStart\""));
        assert!(json.contains("\"identityId\":\"id_123\""));
        assert!(json.contains("\"sessionId\":\"sess_abc\""));
    }

    #[test]
    fn soullog_event_optional_fields_skipped() {
        let event = SoullogEvent {
            id: "test_id".into(),
            service: "nexus-agent".into(),
            topic: "soullog.agent.events".into(),
            event_type: "TurnStart".into(),
            timestamp: "2026-02-16T00:00:00Z".into(),
            identity_id: None,
            session_id: None,
            payload: serde_json::json!({}),
            metadata: SoullogMetadata {
                version: "1.0".into(),
                correlation_id: None,
            },
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("identityId"));
        assert!(!json.contains("sessionId"));
    }

    #[tokio::test]
    async fn client_logs_events_without_panic() {
        // Create client with a non-existent URL — events should be buffered
        // and the flush will fail silently (logged as warn)
        let client = SoullogClient::new(
            "http://127.0.0.1:1/nonexistent",
            "test",
            Some("id_test".into()),
            Some("sess_test".into()),
            10,
            100,
        );

        client.log_event(&AgentEvent::TurnStart { turn: 1 });
        client.log_event(&AgentEvent::Error {
            message: "test error".into(),
        });
        client.log_custom(
            "soullog.agent.custom",
            "CustomEvent",
            serde_json::json!({"key": "value"}),
        );

        // Give flush loop time to process
        tokio::time::sleep(Duration::from_millis(200)).await;
        // No panic = success. Flush errors are handled gracefully.
        drop(client);
    }
}

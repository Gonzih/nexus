//! DelegateTool — recursive subagent spawning via tool interface.
//!
//! The LLM calls `delegate` like any other tool. A child `AgentLoop` is spawned
//! with a purpose-driven tool set, runs autonomously, and returns its result as
//! the tool output. The same `Arc<dyn Provider>` is shared, so rate limits are
//! tracked globally.
//!
//! Recursive delegation is allowed: subagents can themselves call `delegate` to
//! spawn sub-subagents (depth safety valve at 5 to prevent infinite recursion).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;

use soul_core::provider::Provider;
use soul_core::tool::{Tool, ToolOutput, ToolRegistry};
use soul_core::types::{AgentEvent, ToolDefinition};

use crate::agent_helpers::{self, Purpose, DEFAULT_CHILD_TURNS, MAX_CHILD_TURNS, MAX_RESULT_BYTES};

/// Recursive subagent spawning tool.
///
/// Spawns a child `AgentLoop` with purpose-driven tools, runs it to completion,
/// and returns the final text as tool output. Shares the same `Arc<dyn Provider>`
/// for global rate limit tracking.
pub struct DelegateTool {
    provider: Arc<dyn Provider>,
    default_child_turns: usize,
    max_depth: usize,
    current_depth: usize,
    cwd: String,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    parent_session: String,
}

impl DelegateTool {
    pub fn new(
        provider: Arc<dyn Provider>,
        default_child_turns: usize,
        max_depth: usize,
        current_depth: usize,
        cwd: String,
        event_tx: mpsc::UnboundedSender<AgentEvent>,
        parent_session: String,
    ) -> Self {
        Self {
            provider,
            default_child_turns,
            max_depth,
            current_depth,
            cwd,
            event_tx,
            parent_session,
        }
    }

    /// Optionally add a child DelegateTool to the registry if depth allows.
    fn maybe_add_delegate(&self, registry: &mut ToolRegistry) {
        if self.current_depth + 1 < self.max_depth {
            registry.register(Box::new(DelegateTool::new(
                self.provider.clone(),
                self.default_child_turns,
                self.max_depth,
                self.current_depth + 1,
                self.cwd.clone(),
                self.event_tx.clone(),
                self.parent_session.clone(),
            )));
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "delegate".into(),
            description: format!(
                "Spawn a purpose-driven subagent to handle a task autonomously. \
                 The subagent runs with its own tool set and returns the result. \
                 Use this to parallelize work or delegate specialized tasks. \
                 Current depth: {}/{}.",
                self.current_depth, self.max_depth
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The task for the subagent to perform. Be specific and include all necessary context."
                    },
                    "purpose": {
                        "type": "string",
                        "enum": ["research", "explore", "analyze", "code", "general"],
                        "description": "Purpose determines the tool set: research (read+bash for web access), explore (read-only FS navigation), analyze (read-only deep analysis), code (full read/write/edit/bash), general (all tools)."
                    },
                    "max_turns": {
                        "type": "integer",
                        "description": format!(
                            "Maximum turns for the subagent (default: {DEFAULT_CHILD_TURNS}, max: {MAX_CHILD_TURNS}). \
                             Use lower for quick lookups, higher for complex multi-step tasks."
                        )
                    }
                },
                "required": ["task", "purpose"]
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        arguments: serde_json::Value,
        _partial_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> soul_core::error::SoulResult<ToolOutput> {
        // Parse arguments
        let task = arguments
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if task.is_empty() {
            return Ok(ToolOutput::error("delegate: 'task' is required and must not be empty"));
        }

        let purpose = arguments
            .get("purpose")
            .and_then(|v| v.as_str())
            .map(Purpose::from_str)
            .unwrap_or(Purpose::General);

        let max_turns = arguments
            .get("max_turns")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(MAX_CHILD_TURNS))
            .unwrap_or(self.default_child_turns);

        // Generate child session ID
        let child_session = format!(
            "{}-d{}-{}",
            self.parent_session,
            self.current_depth + 1,
            uuid::Uuid::new_v4().to_string().split('-').next().unwrap_or("x"),
        );

        // Build tool registry for purpose (+ optionally add recursive delegate)
        let mut tools = agent_helpers::build_tools(purpose, &self.cwd);
        self.maybe_add_delegate(&mut tools);

        // Emit start event
        let _ = self.event_tx.send(AgentEvent::SubagentStart {
            parent_session: self.parent_session.clone(),
            child_session: child_session.clone(),
            purpose: format!("{:?}", purpose).to_lowercase(),
            max_turns,
        });

        // Build and run child agent
        let model = agent_helpers::make_child_model();
        let config = agent_helpers::make_child_config(model, purpose, max_turns);

        let mut agent = soul_core::agent::AgentLoop::new(self.provider.clone(), tools, config);

        let (child_event_tx, child_event_rx) = mpsc::unbounded_channel();
        let (_steering_tx, steering_rx) = mpsc::unbounded_channel();

        let event_forwarder = agent_helpers::forward_child_events(
            child_event_rx,
            self.event_tx.clone(),
            child_session.clone(),
        );

        let options = soul_core::agent::RunOptions {
            session_id: child_session.clone(),
            initial_messages: vec![soul_core::types::Message::user(&task)],
        };

        let result = agent.run(options, child_event_tx, steering_rx).await;
        event_forwarder.await.ok();

        // Extract result
        let (result_text, turns_used) = agent_helpers::extract_result_text(result);

        // Truncate if needed
        let truncated = agent_helpers::truncate_result(&result_text, MAX_RESULT_BYTES);

        // Emit end event
        let preview = if result_text.len() > 200 {
            format!("{}...", &result_text[..200])
        } else {
            result_text
        };

        let _ = self.event_tx.send(AgentEvent::SubagentEnd {
            child_session,
            purpose: format!("{:?}", purpose).to_lowercase(),
            turns_used,
            result_preview: preview,
        });

        Ok(ToolOutput::success(truncated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soul_core::provider::Provider;
    use soul_core::types::{Message, ModelInfo, ProviderKind};

    fn make_event_tx() -> (mpsc::UnboundedSender<AgentEvent>, mpsc::UnboundedReceiver<AgentEvent>) {
        mpsc::unbounded_channel()
    }

    fn make_delegate(
        event_tx: mpsc::UnboundedSender<AgentEvent>,
        depth: usize,
        max_depth: usize,
    ) -> DelegateTool {
        let provider: Arc<dyn Provider> = Arc::new(MockProvider);
        DelegateTool::new(
            provider,
            DEFAULT_CHILD_TURNS,
            max_depth,
            depth,
            "/tmp".into(),
            event_tx,
            "test-session".into(),
        )
    }

    /// Minimal mock provider for tests that don't need actual LLM calls.
    pub(crate) struct MockProvider;

    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    impl Provider for MockProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Custom("mock".into())
        }

        async fn stream(
            &self,
            _messages: &[Message],
            _system: &str,
            _tools: &[soul_core::types::ToolDefinition],
            _model: &ModelInfo,
            _auth: &soul_core::types::AuthProfile,
            _event_tx: mpsc::UnboundedSender<soul_core::types::StreamDelta>,
        ) -> soul_core::error::SoulResult<Message> {
            Ok(Message::assistant("Mock subagent result"))
        }

        async fn count_tokens(
            &self,
            _messages: &[Message],
            _system: &str,
            _tools: &[soul_core::types::ToolDefinition],
            _model: &ModelInfo,
            _auth: &soul_core::types::AuthProfile,
        ) -> soul_core::error::SoulResult<usize> {
            Ok(100)
        }

        async fn probe(
            &self,
            _model: &ModelInfo,
            _auth: &soul_core::types::AuthProfile,
        ) -> soul_core::error::SoulResult<soul_core::provider::ProbeResult> {
            Ok(soul_core::provider::ProbeResult {
                healthy: true,
                rate_limit_remaining: Some(1.0),
                rate_limit_utilization: Some(0.0),
            })
        }
    }

    // ─── Tool Definition Tests ──────────────────────────────────────────

    #[test]
    fn delegate_tool_definition() {
        let (tx, _rx) = make_event_tx();
        let tool = make_delegate(tx, 0, 5);
        let def = tool.definition();
        assert_eq!(def.name, "delegate");
        assert!(def.description.contains("subagent"));

        let required = def.input_schema["required"].as_array().unwrap();
        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(required_names.contains(&"task"));
        assert!(required_names.contains(&"purpose"));
    }

    #[test]
    fn delegate_tool_name() {
        let (tx, _rx) = make_event_tx();
        let tool = make_delegate(tx, 0, 5);
        assert_eq!(tool.name(), "delegate");
    }

    // ─── Turn Clamping Tests ────────────────────────────────────────────

    #[test]
    fn delegate_clamps_max_turns() {
        let requested: u64 = 100;
        let clamped = (requested as usize).min(MAX_CHILD_TURNS);
        assert_eq!(clamped, 50);
    }

    #[test]
    fn delegate_default_turns() {
        assert_eq!(DEFAULT_CHILD_TURNS, 15);
    }

    // ─── Depth / Recursion Tests ────────────────────────────────────────

    #[test]
    fn delegate_depth_includes_self() {
        let (tx, _rx) = make_event_tx();
        let tool = make_delegate(tx, 0, 5);
        let mut registry = agent_helpers::build_tools(Purpose::General, "/tmp");
        tool.maybe_add_delegate(&mut registry);
        assert!(registry.get("delegate").is_some());
    }

    #[test]
    fn delegate_max_depth_excludes_self() {
        let (tx, _rx) = make_event_tx();
        let tool = make_delegate(tx, 4, 5);
        let mut registry = agent_helpers::build_tools(Purpose::General, "/tmp");
        tool.maybe_add_delegate(&mut registry);
        assert!(registry.get("delegate").is_none());
    }

    #[test]
    fn delegate_at_max_depth_excludes_self() {
        let (tx, _rx) = make_event_tx();
        let tool = make_delegate(tx, 5, 5);
        let mut registry = agent_helpers::build_tools(Purpose::General, "/tmp");
        tool.maybe_add_delegate(&mut registry);
        assert!(registry.get("delegate").is_none());
    }

    #[test]
    fn delegate_depth_1_includes_self() {
        let (tx, _rx) = make_event_tx();
        let tool = make_delegate(tx, 1, 5);
        let mut registry = agent_helpers::build_tools(Purpose::Explore, "/tmp");
        tool.maybe_add_delegate(&mut registry);
        assert!(registry.get("delegate").is_some());
    }

    // ─── Integration Test: Mock Subagent Execution ──────────────────────

    #[tokio::test]
    async fn delegate_executes_mock_subagent() {
        let (tx, mut rx) = make_event_tx();
        let tool = make_delegate(tx, 0, 5);

        let result = tool
            .execute(
                "call_1",
                json!({
                    "task": "Find all Rust files in the project",
                    "purpose": "explore",
                    "max_turns": 3,
                }),
                None,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("Mock subagent result"));

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::SubagentStart { purpose, .. } if purpose == "explore")),
            "Missing SubagentStart event"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::SubagentEnd { .. })),
            "Missing SubagentEnd event"
        );
    }

    #[tokio::test]
    async fn delegate_empty_task_returns_error() {
        let (tx, _rx) = make_event_tx();
        let tool = make_delegate(tx, 0, 5);

        let result = tool
            .execute("call_1", json!({"task": "", "purpose": "explore"}), None)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("required"));
    }

    #[tokio::test]
    async fn delegate_unknown_purpose_defaults_to_general() {
        let (tx, mut rx) = make_event_tx();
        let tool = make_delegate(tx, 0, 5);

        let result = tool
            .execute(
                "call_1",
                json!({"task": "do something", "purpose": "magic"}),
                None,
            )
            .await
            .unwrap();

        assert!(!result.is_error);

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::SubagentStart { purpose, .. } if purpose == "general")));
    }
}

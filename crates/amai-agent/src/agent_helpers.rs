//! Shared helpers for subagent spawning (used by DelegateTool and PlanExecuteTool).
//!
//! Extracted from `delegate.rs` to avoid duplication. Both tools need purpose-driven
//! tool sets, child AgentLoop configuration, event forwarding, and result extraction.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use soul_core::agent::RunOptions;
use soul_core::provider::Provider;
use soul_core::tool::ToolRegistry;
use soul_core::types::{
    AgentConfig, AgentEvent, ContextStrategy, Message, ModelInfo, ProviderKind, Role,
};
use soul_core::vfs::NativeFs;
use soul_core::vexec::NativeExecutor;

/// Maximum result size returned to parent (50KB).
pub const MAX_RESULT_BYTES: usize = 50 * 1024;

/// Results larger than this get summarized by a cheap LLM instead of truncated.
/// Below this threshold, truncation is used (a few extra chars aren't worth an LLM call).
pub const SUMMARY_THRESHOLD_BYTES: usize = 8 * 1024;

/// Default per-child turn limit.
pub const DEFAULT_CHILD_TURNS: usize = 15;

/// Hard ceiling on child turns (even if requested higher).
pub const MAX_CHILD_TURNS: usize = 50;

// ─── Purpose ─────────────────────────────────────────────────────────────────

/// Purpose-driven subagent categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Research,
    Explore,
    Analyze,
    Code,
    General,
}

impl Purpose {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "research" => Purpose::Research,
            "explore" => Purpose::Explore,
            "analyze" | "analysis" => Purpose::Analyze,
            "code" | "coding" => Purpose::Code,
            _ => Purpose::General,
        }
    }

    pub fn system_prompt(&self) -> &'static str {
        match self {
            Purpose::Research => {
                "You are a research agent with bash access for web/network queries. \
                 Use bash to run curl, fetch URLs, or query APIs. Read local files as needed. \
                 Return a compact summary: specific facts, URLs, code snippets. \
                 NO prose. NO preamble. Output only what was asked for."
            }
            Purpose::Explore => {
                "You are a codebase explorer. Your job is TRUTH COMPRESSION: \
                 find the exact facts the parent agent needs, return them as compact pointers. \
                 Output format: 'file_path:line_number — what is there'. \
                 Never return full file contents. Never pad with context. \
                 If asked 'where is X defined' → return one line: 'src/foo.rs:42 — fn X(...)'. \
                 Navigate → find → report. Nothing else."
            }
            Purpose::Analyze => {
                "You are an analysis agent. Read carefully, trace code paths, understand data flow. \
                 Return compact analysis: key patterns, edge cases, risks, architectural decisions. \
                 Format: bullet points with file:line references. No prose padding."
            }
            Purpose::Code => {
                "You are a coding agent. You have the plan — execute it. \
                 Read the files at the locations you were given. Make the specified changes. \
                 After each file change: run cargo check (Rust) or npx tsc --noEmit (TS). \
                 Run tests when all changes are done. Fix failures before stopping. \
                 The loop: READ → CHANGE → CHECK → TEST → FIX → REPEAT. \
                 Stop only when tests pass and compiler is clean."
            }
            Purpose::General => {
                "You are a general-purpose subagent. Complete the task using available tools. \
                 Return a compact result. No preamble, no padding."
            }
        }
    }
}

// ─── Tool Registry Builder ───────────────────────────────────────────────────

/// Build the tool registry for a given purpose and working directory.
pub fn build_tools(purpose: Purpose, cwd: &str) -> ToolRegistry {
    let fs = Arc::new(NativeFs::new(cwd));
    let executor = Arc::new(NativeExecutor);

    match purpose {
        Purpose::Research => {
            // Read-only FS tools + bash (for curl/network access)
            let mut registry = soul_coder::read_only_tools(fs, cwd);
            registry.register(Box::new(soul_coder::BashTool::new(executor, cwd)));
            registry
        }
        Purpose::Explore => {
            // Read-only tools only — no write, no bash
            soul_coder::read_only_tools(fs, cwd)
        }
        Purpose::Analyze => {
            // Read-only tools — careful analysis, no modifications
            soul_coder::read_only_tools(fs, cwd)
        }
        Purpose::Code => {
            // Full tool set — read, write, edit, bash, grep, find, ls
            soul_coder::all_tools(fs, executor, cwd)
        }
        Purpose::General => {
            // Full tool set
            soul_coder::all_tools(fs, executor, cwd)
        }
    }
}

// ─── Child Model & Config ────────────────────────────────────────────────────

/// Create the "balanced" ModelInfo used by child agents.
pub fn make_child_model() -> ModelInfo {
    ModelInfo {
        id: "balanced".into(),
        provider: ProviderKind::Custom("balanced".into()),
        context_window: 128_000,
        max_output_tokens: 8192,
        supports_thinking: false,
        supports_tools: true,
        supports_images: false,
        cost_per_input_token: 0.0,
        cost_per_output_token: 0.0,
    }
}

/// Create an AgentConfig for a child agent.
pub fn make_child_config(model: ModelInfo, purpose: Purpose, max_turns: usize) -> AgentConfig {
    let mut config = AgentConfig::new(model, purpose.system_prompt());
    config.max_turns = Some(max_turns);
    config.context_strategy = ContextStrategy::Classic; // subagents use classic — lightweight
    config
}

// ─── Result Extraction ───────────────────────────────────────────────────────

/// Extract the final result text and turn count from an agent run.
///
/// Returns (result_text, turns_used). On error, returns an error message with 0 turns.
pub fn extract_result_text(
    result: Result<Vec<Message>, soul_core::error::SoulError>,
) -> (String, usize) {
    match result {
        Ok(messages) => {
            let turns = messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .count();

            // Last non-empty assistant message
            let text = messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant && !m.text_content().trim().is_empty())
                .map(|m| m.text_content())
                .unwrap_or_else(|| {
                    // Fallback: concatenate all assistant text
                    messages
                        .iter()
                        .filter(|m| m.role == Role::Assistant)
                        .map(|m| m.text_content())
                        .collect::<Vec<_>>()
                        .join("\n")
                });

            (text, turns)
        }
        Err(e) => (format!("Subagent error: {e}"), 0),
    }
}

/// Truncate text to fit within a byte limit, respecting char boundaries.
pub fn truncate_result(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        text.to_string()
    } else {
        let mut end = max_bytes;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        format!(
            "{}\n\n[... truncated, {} bytes total]",
            &text[..end],
            text.len()
        )
    }
}

/// Compress a large result: summarize via LLM if above threshold, truncate otherwise.
///
/// - `text.len() <= max_bytes` → return as-is
/// - `max_bytes < text.len() <= SUMMARY_THRESHOLD_BYTES` → truncate (not worth LLM call)
/// - `text.len() > SUMMARY_THRESHOLD_BYTES` → one-shot LLM summary, fallback to truncate
pub async fn compress_result(text: &str, max_bytes: usize, provider: &Arc<dyn Provider>) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    if text.len() <= SUMMARY_THRESHOLD_BYTES {
        return truncate_result(text, max_bytes);
    }

    // Large result: summarize with cheapest available model (one-shot, no tools).
    let prompt = format!(
        "Summarize the following agent output in 3-5 bullet points. \
         Preserve: file paths, line numbers, key findings, errors. \
         Be concise. No preamble.\n\n---\n{text}"
    );
    let messages = vec![Message::user(prompt)];
    let model = make_child_model();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

    let auth = soul_core::types::AuthProfile::new(soul_core::types::ProviderKind::Custom("balanced".into()), "");
    match provider.stream(&messages, "", &[], &model, &auth, tx).await {
        Ok(response) => {
            let summary = response.text_content();
            if summary.trim().is_empty() {
                truncate_result(text, max_bytes)
            } else {
                format!("[summarized from {} bytes]\n{}", text.len(), summary)
            }
        }
        Err(_) => truncate_result(text, max_bytes),
    }
}

// ─── Event Forwarding ────────────────────────────────────────────────────────

/// Forward relevant child agent events to the parent event channel.
///
/// Forwards tool execution, structural retry, error, subagent, and cost events.
/// TurnStart is logged at debug level. Other events (message deltas) are dropped
/// to avoid noise.
pub fn forward_child_events(
    mut child_rx: mpsc::UnboundedReceiver<AgentEvent>,
    parent_tx: mpsc::UnboundedSender<AgentEvent>,
    child_session: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = child_rx.recv().await {
            match &event {
                AgentEvent::ToolExecutionStart { .. }
                | AgentEvent::ToolExecutionEnd { .. }
                | AgentEvent::StructuralRetry { .. }
                | AgentEvent::Error { .. }
                | AgentEvent::SubagentStart { .. }
                | AgentEvent::SubagentEnd { .. }
                | AgentEvent::PlanExecutionStart { .. }
                | AgentEvent::PlanTaskStart { .. }
                | AgentEvent::PlanTaskEnd { .. }
                | AgentEvent::PlanExecutionEnd { .. }
                | AgentEvent::Cost(_) => {
                    let _ = parent_tx.send(event);
                }
                AgentEvent::TurnStart { turn } => {
                    tracing::debug!(
                        child = %child_session,
                        turn,
                        "Subagent turn"
                    );
                }
                _ => {} // Don't forward message deltas etc — too noisy
            }
        }
    })
}

// ─── Run a full child AgentLoop ──────────────────────────────────────────────

/// Spawn and run a child AgentLoop to completion. Returns (result_text, turns_used).
///
/// This is the common path for both DelegateTool (single task) and PlanExecuteTool
/// (parallel tasks). Builds tools, config, runs the loop, forwards events, extracts result.
pub async fn run_child_agent(
    provider: Arc<dyn Provider>,
    purpose: Purpose,
    cwd: &str,
    task: &str,
    max_turns: usize,
    parent_event_tx: mpsc::UnboundedSender<AgentEvent>,
    session_id: String,
) -> (String, usize) {
    let tools = build_tools(purpose, cwd);
    let model = make_child_model();
    let config = make_child_config(model, purpose, max_turns);

    let mut agent = soul_core::agent::AgentLoop::new(provider, tools, config);

    let (child_event_tx, child_event_rx) = mpsc::unbounded_channel();
    let (_steering_tx, steering_rx) = mpsc::unbounded_channel();

    let fwd = forward_child_events(child_event_rx, parent_event_tx, session_id.clone());

    let options = RunOptions {
        session_id,
        initial_messages: vec![Message::user(task)],
    };

    let result = agent.run(options, child_event_tx, steering_rx).await;
    fwd.await.ok();

    extract_result_text(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockProvider;

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        fn kind(&self) -> soul_core::types::ProviderKind {
            soul_core::types::ProviderKind::Custom("mock".into())
        }
        async fn stream(
            &self,
            _messages: &[Message],
            _system: &str,
            _tools: &[soul_core::types::ToolDefinition],
            _model: &ModelInfo,
            _auth: &soul_core::types::AuthProfile,
            _event_tx: tokio::sync::mpsc::UnboundedSender<soul_core::types::StreamDelta>,
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
            Ok(soul_core::provider::ProbeResult { healthy: true, rate_limit_remaining: None, rate_limit_utilization: None })
        }
    }

    // ─── Purpose Parsing Tests ──────────────────────────────────────────

    #[test]
    fn purpose_from_str_known() {
        assert_eq!(Purpose::from_str("research"), Purpose::Research);
        assert_eq!(Purpose::from_str("explore"), Purpose::Explore);
        assert_eq!(Purpose::from_str("analyze"), Purpose::Analyze);
        assert_eq!(Purpose::from_str("analysis"), Purpose::Analyze);
        assert_eq!(Purpose::from_str("code"), Purpose::Code);
        assert_eq!(Purpose::from_str("coding"), Purpose::Code);
        assert_eq!(Purpose::from_str("general"), Purpose::General);
    }

    #[test]
    fn purpose_from_str_unknown_defaults_to_general() {
        assert_eq!(Purpose::from_str("nonsense"), Purpose::General);
        assert_eq!(Purpose::from_str(""), Purpose::General);
    }

    #[test]
    fn purpose_from_str_case_insensitive() {
        assert_eq!(Purpose::from_str("RESEARCH"), Purpose::Research);
        assert_eq!(Purpose::from_str("Explore"), Purpose::Explore);
        assert_eq!(Purpose::from_str("CODE"), Purpose::Code);
    }

    // ─── Tool Set Mapping Tests ─────────────────────────────────────────

    #[test]
    fn purpose_to_tools_research() {
        let registry = build_tools(Purpose::Research, "/tmp");
        let names = registry.names();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"grep"));
        assert!(names.contains(&"find"));
        assert!(names.contains(&"ls"));
        assert!(names.contains(&"bash"));
        assert!(!names.contains(&"write"));
        assert!(!names.contains(&"edit"));
    }

    #[test]
    fn purpose_to_tools_explore() {
        let registry = build_tools(Purpose::Explore, "/tmp");
        let names = registry.names();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"grep"));
        assert!(names.contains(&"find"));
        assert!(names.contains(&"ls"));
        assert!(!names.contains(&"write"));
        assert!(!names.contains(&"edit"));
        assert!(!names.contains(&"bash"));
    }

    #[test]
    fn purpose_to_tools_analyze() {
        let registry = build_tools(Purpose::Analyze, "/tmp");
        let names = registry.names();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"grep"));
        assert!(names.contains(&"find"));
        assert!(names.contains(&"ls"));
        assert!(!names.contains(&"write"));
        assert!(!names.contains(&"bash"));
    }

    #[test]
    fn purpose_to_tools_code() {
        let registry = build_tools(Purpose::Code, "/tmp");
        let names = registry.names();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"write"));
        assert!(names.contains(&"edit"));
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"grep"));
        assert!(names.contains(&"find"));
        assert!(names.contains(&"ls"));
    }

    #[test]
    fn purpose_to_tools_general() {
        let registry = build_tools(Purpose::General, "/tmp");
        let names = registry.names();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"write"));
        assert!(names.contains(&"edit"));
        assert!(names.contains(&"bash"));
    }

    // ─── System Prompt Tests ────────────────────────────────────────────

    #[test]
    fn each_purpose_has_system_prompt() {
        let purposes = [
            Purpose::Research,
            Purpose::Explore,
            Purpose::Analyze,
            Purpose::Code,
            Purpose::General,
        ];
        for p in purposes {
            let prompt = p.system_prompt();
            assert!(!prompt.is_empty(), "Purpose {:?} has empty system prompt", p);
            assert!(
                prompt.len() > 50,
                "Purpose {:?} system prompt too short: {}",
                p,
                prompt.len()
            );
        }
    }

    // ─── Result Extraction Tests ────────────────────────────────────────

    // ─── compress_result Tests ──────────────────────────────────────────

    #[tokio::test]
    async fn compress_result_passthrough_under_limit() {
        let provider: Arc<dyn Provider> = Arc::new(MockProvider);
        let text = "short result";
        let result = compress_result(text, 100, &provider).await;
        assert_eq!(result, "short result");
    }

    #[tokio::test]
    async fn compress_result_truncates_below_summary_threshold() {
        let provider: Arc<dyn Provider> = Arc::new(MockProvider);
        // text.len() > max_bytes but <= SUMMARY_THRESHOLD_BYTES → truncate path
        let text = "a".repeat(200);
        let result = compress_result(&text, 50, &provider).await;
        assert!(result.contains("truncated"));
        assert!(!result.contains("summarized"));
    }

    #[tokio::test]
    async fn compress_result_summarizes_above_threshold() {
        let provider: Arc<dyn Provider> = Arc::new(MockProvider);
        // text.len() > SUMMARY_THRESHOLD_BYTES → LLM summary path
        let text = "x".repeat(SUMMARY_THRESHOLD_BYTES + 1);
        let result = compress_result(&text, 100, &provider).await;
        // MockProvider returns "Mock subagent result" → should get summarized header
        assert!(result.contains("summarized from"));
    }

    #[tokio::test]
    async fn compress_result_fallback_on_provider_error() {
        let provider: Arc<dyn Provider> = Arc::new(ErrorProvider);
        let text = "x".repeat(SUMMARY_THRESHOLD_BYTES + 1);
        let result = compress_result(&text, 100, &provider).await;
        // Provider errors → falls back to truncation
        assert!(result.contains("truncated"));
    }

    /// Provider that always errors — for testing compress_result fallback.
    struct ErrorProvider;

    #[async_trait::async_trait]
    impl Provider for ErrorProvider {
        fn kind(&self) -> soul_core::types::ProviderKind {
            soul_core::types::ProviderKind::Custom("error".into())
        }
        async fn stream(
            &self,
            _messages: &[Message],
            _system: &str,
            _tools: &[soul_core::types::ToolDefinition],
            _model: &ModelInfo,
            _auth: &soul_core::types::AuthProfile,
            _event_tx: tokio::sync::mpsc::UnboundedSender<soul_core::types::StreamDelta>,
        ) -> soul_core::error::SoulResult<Message> {
            Err(soul_core::error::SoulError::Provider("simulated failure".into()))
        }
        async fn count_tokens(
            &self,
            _messages: &[Message],
            _system: &str,
            _tools: &[soul_core::types::ToolDefinition],
            _model: &ModelInfo,
            _auth: &soul_core::types::AuthProfile,
        ) -> soul_core::error::SoulResult<usize> {
            Ok(0)
        }
        async fn probe(
            &self,
            _model: &ModelInfo,
            _auth: &soul_core::types::AuthProfile,
        ) -> soul_core::error::SoulResult<soul_core::provider::ProbeResult> {
            Ok(soul_core::provider::ProbeResult { healthy: false, rate_limit_remaining: None, rate_limit_utilization: None })
        }
    }

    // ─── truncate_result Tests ──────────────────────────────────────────

    #[test]
    fn truncate_result_within_limit() {
        let text = "hello world";
        let result = truncate_result(text, 100);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn truncate_result_over_limit() {
        let text = "hello world, this is a long string";
        let result = truncate_result(text, 11);
        assert!(result.starts_with("hello world"));
        assert!(result.contains("truncated"));
        assert!(result.contains(&text.len().to_string()));
    }

    #[test]
    fn extract_result_text_success() {
        let messages = vec![
            Message::user("do something"),
            Message::assistant("I did it"),
        ];
        let (text, turns) = extract_result_text(Ok(messages));
        assert_eq!(text, "I did it");
        assert_eq!(turns, 1);
    }

    #[test]
    fn extract_result_text_error() {
        let err = soul_core::error::SoulError::Provider("boom".into());
        let (text, turns) = extract_result_text(Err(err));
        assert!(text.contains("Subagent error"));
        assert!(text.contains("boom"));
        assert_eq!(turns, 0);
    }

    #[test]
    fn extract_result_text_multiple_assistants() {
        let messages = vec![
            Message::user("task"),
            Message::assistant("first response"),
            Message::user("follow up"),
            Message::assistant("second response"),
        ];
        let (text, turns) = extract_result_text(Ok(messages));
        assert_eq!(text, "second response"); // last non-empty assistant
        assert_eq!(turns, 2);
    }

    // ─── Child Model Tests ──────────────────────────────────────────────

    #[test]
    fn make_child_model_is_balanced() {
        let model = make_child_model();
        assert_eq!(model.id, "balanced");
        assert!(model.supports_tools);
        assert_eq!(model.context_window, 128_000);
    }

    #[test]
    fn make_child_config_sets_classic_strategy() {
        let model = make_child_model();
        let config = make_child_config(model, Purpose::Code, 20);
        assert_eq!(config.context_strategy, ContextStrategy::Classic);
        assert_eq!(config.max_turns, Some(20));
    }
}

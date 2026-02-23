//! PlanExecuteTool — parallel task graph execution with sub-agents.
//!
//! The LLM calls `plan_execute` with a JSON task graph. The tool:
//! 1. Parses the graph into a `Planner` DAG (cycle detection, dependency wiring)
//! 2. Spawns full `AgentLoop` sub-agents for ready tasks in parallel via `tokio::spawn`
//! 3. Waits for each wave to complete, unblocks dependents, repeats
//! 4. Returns aggregated results from all tasks
//!
//! Each sub-agent gets its own provider/tools/context and runs autonomously.
//! The same `Arc<dyn Provider>` is shared for global rate limit tracking.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;

use soul_core::planner::{Planner, TaskId, TaskStatus};
use soul_core::provider::Provider;
use soul_core::tool::{Tool, ToolOutput};
use soul_core::types::{AgentEvent, ToolDefinition};

use crate::agent_helpers::{self, Purpose, MAX_RESULT_BYTES};

/// Default maximum parallel sub-agents per wave.
const DEFAULT_MAX_PARALLEL: usize = 4;

/// Default turn limit per sub-agent task.
const DEFAULT_TURNS_PER_TASK: usize = 25;

/// Parallel task graph execution tool.
///
/// Accepts a JSON task graph from the LLM, builds a DAG via `Planner`,
/// spawns parallel `AgentLoop` sub-agents for independent tasks, and
/// returns aggregated results.
pub struct PlanExecuteTool {
    provider: Arc<dyn Provider>,
    cwd: String,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    parent_session: String,
    max_parallel: usize,
    max_turns_per_task: usize,
}

impl PlanExecuteTool {
    pub fn new(
        provider: Arc<dyn Provider>,
        cwd: String,
        event_tx: mpsc::UnboundedSender<AgentEvent>,
        parent_session: String,
    ) -> Self {
        Self {
            provider,
            cwd,
            event_tx,
            parent_session,
            max_parallel: DEFAULT_MAX_PARALLEL,
            max_turns_per_task: DEFAULT_TURNS_PER_TASK,
        }
    }

    /// Parse a JSON task graph into a Planner DAG.
    ///
    /// Returns (Planner, id_to_string map, string_to_purpose map).
    fn parse_task_graph(
        &self,
        tasks_json: &serde_json::Value,
    ) -> Result<(Planner, HashMap<TaskId, String>, HashMap<TaskId, Purpose>), String> {
        let tasks = tasks_json
            .as_array()
            .ok_or("'tasks' must be an array")?;

        if tasks.is_empty() {
            return Err("'tasks' array must not be empty".into());
        }

        let mut planner = Planner::new();
        let mut str_to_id: HashMap<String, TaskId> = HashMap::new();
        let mut id_to_str: HashMap<TaskId, String> = HashMap::new();
        let mut id_to_purpose: HashMap<TaskId, Purpose> = HashMap::new();

        // First pass: create all tasks
        for task in tasks {
            let str_id = task
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or("Each task must have a string 'id'")?
                .to_string();

            if str_to_id.contains_key(&str_id) {
                return Err(format!("Duplicate task id: '{str_id}'"));
            }

            let description = task
                .get("description")
                .and_then(|v| v.as_str())
                .ok_or(format!("Task '{str_id}' must have a 'description'"))?
                .to_string();

            let purpose = task
                .get("purpose")
                .and_then(|v| v.as_str())
                .map(Purpose::from_str)
                .unwrap_or(Purpose::General);

            let active_form = format!("Running: {str_id}");
            let task_id = planner.add_task_with_description(
                &str_id,
                &description,
                Some(active_form),
            );

            str_to_id.insert(str_id.clone(), task_id);
            id_to_str.insert(task_id, str_id);
            id_to_purpose.insert(task_id, purpose);
        }

        // Second pass: wire dependencies
        for task in tasks {
            let str_id = task["id"].as_str().unwrap();
            let task_id = str_to_id[str_id];

            if let Some(deps) = task.get("depends_on").and_then(|v| v.as_array()) {
                for dep in deps {
                    let dep_str = dep
                        .as_str()
                        .ok_or("depends_on entries must be strings")?;
                    let dep_id = str_to_id
                        .get(dep_str)
                        .ok_or(format!(
                            "Task '{str_id}' depends on unknown task '{dep_str}'"
                        ))?;
                    planner
                        .add_dependency(task_id, *dep_id)
                        .map_err(|e| format!("Dependency error: {e}"))?;
                }
            }
        }

        Ok((planner, id_to_str, id_to_purpose))
    }

    /// Spawn a sub-agent for a single task. Returns a JoinHandle with (task_id, result_text_or_error).
    fn spawn_task_agent(
        &self,
        task_id: TaskId,
        str_id: &str,
        description: &str,
        purpose: Purpose,
    ) -> tokio::task::JoinHandle<(TaskId, Result<String, String>)> {
        let provider = self.provider.clone();
        let cwd = self.cwd.clone();
        let parent_tx = self.event_tx.clone();
        let session_id = format!("{}-plan-{}", self.parent_session, str_id);
        let max_turns = self.max_turns_per_task;
        let description = description.to_string();

        tokio::spawn(async move {
            let (text, _turns) = agent_helpers::run_child_agent(
                provider,
                purpose,
                &cwd,
                &description,
                max_turns,
                parent_tx,
                session_id,
            )
            .await;

            let truncated = agent_helpers::truncate_result(&text, MAX_RESULT_BYTES);
            (task_id, Ok(truncated))
        })
    }

    /// Propagate failures: skip any pending task whose blocked_by set contains
    /// a failed or skipped task.
    fn propagate_failures(planner: &mut Planner) {
        // Collect IDs to skip (can't mutate while iterating)
        loop {
            let to_skip: Vec<TaskId> = planner
                .all_tasks()
                .iter()
                .filter(|t| t.status == TaskStatus::Pending)
                .filter(|t| {
                    t.blocked_by.iter().any(|dep_id| {
                        planner
                            .get(*dep_id)
                            .map(|dep| {
                                dep.status == TaskStatus::Failed
                                    || dep.status == TaskStatus::Skipped
                            })
                            .unwrap_or(false)
                    })
                })
                .map(|t| t.id)
                .collect();

            if to_skip.is_empty() {
                break;
            }

            for id in to_skip {
                let _ = planner.skip(id);
            }
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl Tool for PlanExecuteTool {
    fn name(&self) -> &str {
        "plan_execute"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "plan_execute".into(),
            description: "Execute a task graph as subagents. Supports sequential (depends_on) and \
                         parallel (independent) tasks. Primary use: Phase 3 implementation after \
                         research and planning are done. Each task gets a purpose-driven tool set. \
                         Keep task descriptions surgical: give file:line targets and what to change, \
                         not research dumps. Returns aggregated results."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "description": "Array of tasks forming a dependency graph (DAG). Independent tasks run in parallel.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "description": "Unique task identifier (descriptive name, e.g. 'setup-db', 'write-tests')"
                                },
                                "description": {
                                    "type": "string",
                                    "description": "Detailed task description — this becomes the sub-agent's prompt"
                                },
                                "purpose": {
                                    "type": "string",
                                    "enum": ["research", "explore", "analyze", "code", "general"],
                                    "description": "Determines tool set: research (read+bash), explore (read-only), analyze (read-only), code (full tools), general (all tools)"
                                },
                                "depends_on": {
                                    "type": "array",
                                    "items": {"type": "string"},
                                    "description": "IDs of tasks that must complete before this one starts"
                                }
                            },
                            "required": ["id", "description", "purpose"]
                        }
                    }
                },
                "required": ["tasks"]
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        arguments: serde_json::Value,
        _partial_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> soul_core::error::SoulResult<ToolOutput> {
        // Parse task graph
        let tasks_json = match arguments.get("tasks") {
            Some(v) => v,
            None => return Ok(ToolOutput::error("plan_execute: 'tasks' field is required")),
        };

        let (mut planner, id_to_str, id_to_purpose) = match self.parse_task_graph(tasks_json) {
            Ok(v) => v,
            Err(e) => return Ok(ToolOutput::error(format!("plan_execute: {e}"))),
        };

        let total = planner.len();

        // Emit start event
        let _ = self.event_tx.send(AgentEvent::PlanExecutionStart {
            parent_session: self.parent_session.clone(),
            task_count: total,
        });

        // Store results per task
        let mut results: HashMap<TaskId, String> = HashMap::new();

        // Parallel execution loop
        loop {
            if planner.is_done() {
                break;
            }

            // Get ready tasks (pending + unblocked)
            let ready: Vec<TaskId> = planner
                .ready_tasks()
                .iter()
                .map(|t| t.id)
                .take(self.max_parallel)
                .collect();

            if ready.is_empty() {
                // Check for deadlock: tasks remain but none are ready
                let non_terminal: Vec<TaskId> = planner
                    .all_tasks()
                    .iter()
                    .filter(|t| !t.status.is_terminal())
                    .map(|t| t.id)
                    .collect();

                if !non_terminal.is_empty() {
                    // Deadlock — fail remaining tasks
                    for id in non_terminal {
                        let _ = planner.fail_with_error(id, "Deadlock: no tasks can proceed");
                    }
                }
                break;
            }

            // Start tasks and spawn agents
            let mut handles = Vec::new();
            for task_id in &ready {
                let _ = planner.start(*task_id);

                let str_id = &id_to_str[task_id];
                let description = planner
                    .get(*task_id)
                    .and_then(|t| t.description.as_deref())
                    .unwrap_or(&planner.get(*task_id).unwrap().subject)
                    .to_string();
                let purpose = id_to_purpose.get(task_id).copied().unwrap_or(Purpose::General);

                // Emit task start
                let _ = self.event_tx.send(AgentEvent::PlanTaskStart {
                    parent_session: self.parent_session.clone(),
                    task_id: str_id.clone(),
                    description: description.clone(),
                });

                handles.push(self.spawn_task_agent(*task_id, str_id, &description, purpose));
            }

            // Wait for all handles in this wave
            let wave_results = futures::future::join_all(handles).await;

            for join_result in wave_results {
                match join_result {
                    Ok((task_id, Ok(text))) => {
                        let str_id = &id_to_str[&task_id];
                        let _ = planner.complete(task_id);
                        let _ = planner.checkpoint(task_id, &text);
                        results.insert(task_id, text.clone());

                        let preview = if text.len() > 200 {
                            format!("{}...", &text[..200])
                        } else {
                            text
                        };

                        let _ = self.event_tx.send(AgentEvent::PlanTaskEnd {
                            task_id: str_id.clone(),
                            status: "completed".into(),
                            result_preview: preview,
                        });
                    }
                    Ok((task_id, Err(err))) => {
                        let str_id = &id_to_str[&task_id];
                        let _ = planner.fail_with_error(task_id, &err);
                        results.insert(task_id, format!("FAILED: {err}"));

                        let _ = self.event_tx.send(AgentEvent::PlanTaskEnd {
                            task_id: str_id.clone(),
                            status: "failed".into(),
                            result_preview: err,
                        });
                    }
                    Err(join_err) => {
                        // Tokio JoinError — task panicked or was cancelled.
                        // We need to figure out which task this was. Since join_all
                        // preserves order, we can't easily correlate. Log and continue.
                        tracing::error!(error = %join_err, "Sub-agent task panicked");
                    }
                }
            }

            // Propagate failures to dependent tasks
            Self::propagate_failures(&mut planner);
        }

        // Aggregate results
        let counts = planner.counts();
        let topo = planner.topological_order().unwrap_or_default();

        let mut output = String::new();
        for task_id in &topo {
            let str_id = &id_to_str[task_id];
            let status = planner
                .get(*task_id)
                .map(|t| format!("{:?}", t.status))
                .unwrap_or_else(|| "unknown".into());

            let result_preview = results
                .get(task_id)
                .map(|r| {
                    if r.len() > 500 {
                        format!("{}...", &r[..500])
                    } else {
                        r.clone()
                    }
                })
                .unwrap_or_else(|| "(no output)".into());

            output.push_str(&format!("\n## {str_id} [{status}]\n{result_preview}\n"));
        }

        output.push_str(&format!(
            "\n---\nPlan complete: {}/{} succeeded, {} failed, {} skipped\n",
            counts.completed, counts.total, counts.failed, counts.skipped,
        ));

        // Emit end event
        let _ = self.event_tx.send(AgentEvent::PlanExecutionEnd {
            completed: counts.completed,
            failed: counts.failed,
            skipped: counts.skipped,
            total: counts.total,
        });

        // Truncate total output
        let truncated = agent_helpers::truncate_result(&output, MAX_RESULT_BYTES);
        Ok(ToolOutput::success(truncated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use soul_core::types::{AgentEvent, Message, ModelInfo, ProviderKind};

    /// Minimal mock provider for tests — returns a simple text response.
    struct MockProvider;

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

    fn make_plan_execute(
        event_tx: mpsc::UnboundedSender<AgentEvent>,
    ) -> PlanExecuteTool {
        let provider: Arc<dyn Provider> = Arc::new(MockProvider);
        PlanExecuteTool::new(
            provider,
            "/tmp".into(),
            event_tx,
            "test-session".into(),
        )
    }

    // ─── Tool Definition Tests ──────────────────────────────────────────

    #[test]
    fn plan_execute_tool_definition() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let tool = make_plan_execute(tx);
        let def = tool.definition();
        assert_eq!(def.name, "plan_execute");
        assert!(def.description.contains("subagent"));

        let required = def.input_schema["required"].as_array().unwrap();
        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(required_names.contains(&"tasks"));
    }

    #[test]
    fn plan_execute_tool_name() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let tool = make_plan_execute(tx);
        assert_eq!(tool.name(), "plan_execute");
    }

    // ─── Parse Graph Tests ──────────────────────────────────────────────

    #[test]
    fn parse_task_graph_basic() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let tool = make_plan_execute(tx);

        let tasks = json!([
            {"id": "task-a", "description": "Do thing A", "purpose": "code"},
            {"id": "task-b", "description": "Do thing B", "purpose": "explore"}
        ]);

        let (planner, id_to_str, id_to_purpose) = tool.parse_task_graph(&tasks).unwrap();
        assert_eq!(planner.len(), 2);
        assert_eq!(planner.ready_tasks().len(), 2); // both independent = both ready

        // Verify mappings
        assert!(id_to_str.values().any(|v| v == "task-a"));
        assert!(id_to_str.values().any(|v| v == "task-b"));
        assert!(id_to_purpose.values().any(|p| *p == Purpose::Code));
        assert!(id_to_purpose.values().any(|p| *p == Purpose::Explore));
    }

    #[test]
    fn parse_task_graph_with_deps() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let tool = make_plan_execute(tx);

        let tasks = json!([
            {"id": "setup", "description": "Set up project", "purpose": "code", "depends_on": []},
            {"id": "build", "description": "Build project", "purpose": "code", "depends_on": ["setup"]}
        ]);

        let (planner, _, _) = tool.parse_task_graph(&tasks).unwrap();
        assert_eq!(planner.len(), 2);
        assert_eq!(planner.ready_tasks().len(), 1); // only "setup" is ready
        assert_eq!(planner.ready_tasks()[0].subject, "setup");
    }

    #[test]
    fn parse_task_graph_cycle_rejected() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let tool = make_plan_execute(tx);

        let tasks = json!([
            {"id": "a", "description": "Task A", "purpose": "code", "depends_on": ["b"]},
            {"id": "b", "description": "Task B", "purpose": "code", "depends_on": ["a"]}
        ]);

        let result = tool.parse_task_graph(&tasks);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.contains("cycle") || err.contains("Dependency"), "Got: {err}");
    }

    #[test]
    fn parse_task_graph_missing_dep() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let tool = make_plan_execute(tx);

        let tasks = json!([
            {"id": "a", "description": "Task A", "purpose": "code", "depends_on": ["nonexistent"]}
        ]);

        let result = tool.parse_task_graph(&tasks);
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("unknown task"));
    }

    #[test]
    fn parse_task_graph_empty() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let tool = make_plan_execute(tx);

        let tasks = json!([]);
        let result = tool.parse_task_graph(&tasks);
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("empty"));
    }

    #[test]
    fn parse_task_graph_duplicate_ids() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let tool = make_plan_execute(tx);

        let tasks = json!([
            {"id": "same", "description": "First", "purpose": "code"},
            {"id": "same", "description": "Second", "purpose": "code"}
        ]);

        let result = tool.parse_task_graph(&tasks);
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("Duplicate"));
    }

    // ─── Integration Tests: Mock Execution ──────────────────────────────

    #[tokio::test]
    async fn plan_execute_mock_parallel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tool = make_plan_execute(tx);

        let result = tool
            .execute(
                "call_1",
                json!({
                    "tasks": [
                        {"id": "task-a", "description": "Do thing A", "purpose": "explore"},
                        {"id": "task-b", "description": "Do thing B", "purpose": "explore"}
                    ]
                }),
                None,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("task-a"));
        assert!(result.content.contains("task-b"));
        assert!(result.content.contains("2/2 succeeded"));

        // Check events
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::PlanExecutionStart { task_count: 2, .. }
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::PlanExecutionEnd { completed: 2, total: 2, .. }
        )));
    }

    #[tokio::test]
    async fn plan_execute_mock_sequential() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let tool = make_plan_execute(tx);

        let result = tool
            .execute(
                "call_1",
                json!({
                    "tasks": [
                        {"id": "first", "description": "Do first thing", "purpose": "code", "depends_on": []},
                        {"id": "second", "description": "Do second thing", "purpose": "code", "depends_on": ["first"]}
                    ]
                }),
                None,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("first"));
        assert!(result.content.contains("second"));
        assert!(result.content.contains("2/2 succeeded"));
    }

    #[tokio::test]
    async fn plan_execute_empty_tasks_returns_error() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let tool = make_plan_execute(tx);

        let result = tool
            .execute("call_1", json!({"tasks": []}), None)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("empty"));
    }

    #[tokio::test]
    async fn plan_execute_missing_tasks_field() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let tool = make_plan_execute(tx);

        let result = tool
            .execute("call_1", json!({}), None)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("required"));
    }
}

//! Context Strategy Benchmark — RLM vs SemanticGraph vs Classic
//!
//! Runs the same agent task under all three context strategies and captures
//! detailed metrics for comparison: token efficiency, turn count, tool usage,
//! timing, and strategy-specific stats (RLM context doc size, graph node/edge counts).

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use soul_core::agent::{AgentLoop, RunOptions};
use soul_core::rlm::RlmEngine;
use soul_core::semantic_recursion::SemanticContextEngine;
use soul_core::types::*;

/// Metrics captured from a single benchmark run
#[derive(Debug, Clone)]
pub struct BenchMetrics {
    pub strategy: String,
    pub task: String,

    // Timing
    pub total_duration: Duration,
    pub per_turn_latency: Vec<Duration>,

    // Turns & completion
    pub total_turns: usize,
    pub completed: bool,
    pub final_text: String,

    // Token usage (aggregated from events)
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_tokens: u64,

    // Tool usage
    pub tool_calls: Vec<(String, usize)>, // (tool_name, count)
    pub total_tool_calls: usize,
    pub tool_errors: usize,

    // Compaction (Classic only)
    pub compactions: usize,
    pub tokens_before_compaction: Vec<usize>,
    pub tokens_after_compaction: Vec<usize>,

    // Context-specific
    pub context_doc_chars: usize,    // RLM: serialized doc size
    pub context_doc_lines: usize,    // RLM: serialized doc lines
    pub context_doc_est_tokens: usize, // RLM: estimated tokens in full doc
    pub graph_nodes: usize,          // SemanticGraph: total nodes
    pub graph_active_nodes: usize,   // SemanticGraph: active nodes
    pub graph_edges: usize,          // SemanticGraph: total edges
    pub graph_symlinks: usize,       // SemanticGraph: symlink count
    pub graph_tokens_saved: usize,   // SemanticGraph: tokens saved via symlinks
    pub graph_vector_entries: usize,  // SemanticGraph: vector store size
    pub graph_vocab_size: usize,     // SemanticGraph: tokenizer vocab
}

impl BenchMetrics {
    fn new(strategy: &str, task: &str) -> Self {
        Self {
            strategy: strategy.into(),
            task: task.into(),
            total_duration: Duration::ZERO,
            per_turn_latency: Vec::new(),
            total_turns: 0,
            completed: false,
            final_text: String::new(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_tokens: 0,
            tool_calls: Vec::new(),
            total_tool_calls: 0,
            tool_errors: 0,
            compactions: 0,
            tokens_before_compaction: Vec::new(),
            tokens_after_compaction: Vec::new(),
            context_doc_chars: 0,
            context_doc_lines: 0,
            context_doc_est_tokens: 0,
            graph_nodes: 0,
            graph_active_nodes: 0,
            graph_edges: 0,
            graph_symlinks: 0,
            graph_tokens_saved: 0,
            graph_vector_entries: 0,
            graph_vocab_size: 0,
        }
    }
}

/// Run a single benchmark with the given strategy
pub async fn run_benchmark(
    provider: Arc<dyn soul_core::provider::Provider>,
    strategy: ContextStrategy,
    task: &str,
    max_turns: usize,
    cwd: &str,
) -> BenchMetrics {
    let strategy_name = match strategy {
        ContextStrategy::Classic => "classic",
        ContextStrategy::Rlm => "rlm",
        ContextStrategy::SemanticGraph => "semantic_graph",
    };

    tracing::info!(
        strategy = strategy_name,
        task = %truncate_str(task, 80),
        max_turns,
        "Starting benchmark"
    );

    let mut metrics = BenchMetrics::new(strategy_name, task);

    // Build tools
    let fs = Arc::new(soul_core::vfs::NativeFs::new(cwd));
    let executor = Arc::new(soul_core::vexec::NativeExecutor);
    let exec_registry = soul_coder::all_executor(fs.clone(), executor.clone(), cwd);

    let tool_registry = soul_coder::all_tools(
        Arc::new(soul_core::vfs::NativeFs::new(cwd)),
        Arc::new(soul_core::vexec::NativeExecutor),
        cwd,
    );

    let model = ModelInfo {
        id: "balanced".into(),
        provider: ProviderKind::Custom("balanced".into()),
        context_window: 128_000,
        max_output_tokens: 8192,
        supports_thinking: false,
        supports_tools: true,
        supports_images: false,
        cost_per_input_token: 0.0,
        cost_per_output_token: 0.0,
    };

    let mut agent_config = AgentConfig::new(model, crate::prompt::SYSTEM_PROMPT);
    agent_config.max_turns = Some(max_turns);
    agent_config.context_strategy = strategy;

    let mut agent = AgentLoop::new(provider, tool_registry, agent_config)
        .with_executor_registry(exec_registry);

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_steering_tx, steering_rx) = mpsc::unbounded_channel();

    let session_id = format!("bench-{strategy_name}-{}", chrono_millis());

    let options = RunOptions {
        session_id,
        initial_messages: vec![Message::user(task)],
    };

    // Collect events in background
    let event_collector = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = event_rx.recv().await {
            events.push(event);
        }
        events
    });

    let start = Instant::now();
    let result = agent.run(options, event_tx, steering_rx).await;
    metrics.total_duration = start.elapsed();

    // Collect events
    let events = event_collector.await.unwrap_or_default();

    // Process events into metrics
    let mut tool_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut current_turn_start = start;

    for event in &events {
        match event {
            AgentEvent::TurnStart { turn } => {
                if *turn > 0 {
                    metrics.per_turn_latency.push(current_turn_start.elapsed());
                }
                current_turn_start = Instant::now();
                metrics.total_turns = *turn + 1;
            }
            AgentEvent::ToolExecutionStart { tool_name, .. } => {
                *tool_counts.entry(tool_name.clone()).or_default() += 1;
                metrics.total_tool_calls += 1;
            }
            AgentEvent::ToolExecutionEnd { result, .. } => {
                if let ContentBlock::ToolResult { is_error, .. } = result {
                    if *is_error {
                        metrics.tool_errors += 1;
                    }
                }
            }
            AgentEvent::CompactionStart { tokens_before, .. } => {
                metrics.compactions += 1;
                metrics.tokens_before_compaction.push(*tokens_before);
            }
            AgentEvent::CompactionEnd { tokens_after, .. } => {
                metrics.tokens_after_compaction.push(*tokens_after);
            }
            AgentEvent::Cost(cost_event) => {
                metrics.total_input_tokens += cost_event.input_tokens;
                metrics.total_output_tokens += cost_event.output_tokens;
            }
            AgentEvent::MessageEnd { message } => {
                if let Some(usage) = &message.usage {
                    // Fallback if cost events don't fire
                    if metrics.total_input_tokens == 0 {
                        metrics.total_input_tokens += usage.input_tokens as u64;
                        metrics.total_output_tokens += usage.output_tokens as u64;
                    }
                }
            }
            AgentEvent::AgentEnd { messages, .. } => {
                metrics.completed = true;
                if let Some(last) = messages.last() {
                    metrics.final_text = last.text_content();
                }

                // Compute strategy-specific metrics from final messages
                match strategy_name {
                    "rlm" => {
                        let doc = RlmEngine::serialize_conversation(messages);
                        metrics.context_doc_chars = doc.len();
                        metrics.context_doc_lines = doc.lines().count();
                        metrics.context_doc_est_tokens = doc.len() / 4;
                    }
                    "semantic_graph" => {
                        // We'd need access to the engine — compute from message history instead
                        let mut engine = SemanticContextEngine::new();
                        for msg in messages {
                            match msg.role {
                                Role::User => {
                                    engine.ingest_user_request(&msg.text_content());
                                }
                                Role::Assistant => {
                                    let text = msg.text_content();
                                    if !text.is_empty() {
                                        engine.ingest_llm_response(&text, 0, Some("balanced"));
                                    }
                                }
                                _ => {}
                            }
                        }
                        let stats = engine.stats();
                        metrics.graph_nodes = stats.total_nodes;
                        metrics.graph_active_nodes = stats.active_nodes;
                        metrics.graph_edges = stats.total_edges;
                        metrics.graph_symlinks = stats.symlink_count;
                        metrics.graph_tokens_saved = stats.symlink_tokens_saved;
                        metrics.graph_vector_entries = stats.vector_entries;
                        metrics.graph_vocab_size = stats.vocab_size;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // If AgentEnd didn't fire (error case), get final text from result
    if !metrics.completed {
        if let Ok(ref messages) = result {
            metrics.completed = true;
            if let Some(last) = messages.last() {
                metrics.final_text = last.text_content();
            }

            match strategy_name {
                "rlm" => {
                    let doc = RlmEngine::serialize_conversation(messages);
                    metrics.context_doc_chars = doc.len();
                    metrics.context_doc_lines = doc.lines().count();
                    metrics.context_doc_est_tokens = doc.len() / 4;
                }
                "semantic_graph" => {
                    let mut engine = SemanticContextEngine::new();
                    for msg in messages {
                        match msg.role {
                            Role::User => {
                                engine.ingest_user_request(&msg.text_content());
                            }
                            Role::Assistant => {
                                let text = msg.text_content();
                                if !text.is_empty() {
                                    engine.ingest_llm_response(&text, 0, Some("balanced"));
                                }
                            }
                            _ => {}
                        }
                    }
                    let stats = engine.stats();
                    metrics.graph_nodes = stats.total_nodes;
                    metrics.graph_active_nodes = stats.active_nodes;
                    metrics.graph_edges = stats.total_edges;
                    metrics.graph_vector_entries = stats.vector_entries;
                    metrics.graph_vocab_size = stats.vocab_size;
                }
                _ => {}
            }
        }
    }

    metrics.total_tokens = metrics.total_input_tokens + metrics.total_output_tokens;

    // Build tool counts list
    let mut tool_list: Vec<(String, usize)> = tool_counts.into_iter().collect();
    tool_list.sort_by(|a, b| b.1.cmp(&a.1));
    metrics.tool_calls = tool_list;

    // Push last turn latency
    if metrics.total_turns > 0 {
        metrics.per_turn_latency.push(current_turn_start.elapsed());
    }

    // Print summary to stderr
    print_summary(&metrics);

    metrics
}

fn print_summary(m: &BenchMetrics) {
    tracing::info!(
        strategy = %m.strategy,
        duration = ?m.total_duration,
        turns = m.total_turns,
        completed = m.completed,
        total_tokens = m.total_tokens,
        input_tokens = m.total_input_tokens,
        output_tokens = m.total_output_tokens,
        tool_calls = m.total_tool_calls,
        tool_errors = m.tool_errors,
        "Benchmark summary"
    );

    for (name, count) in &m.tool_calls {
        tracing::debug!(tool = %name, count, "Tool usage");
    }

    if m.compactions > 0 {
        tracing::info!(compactions = m.compactions, "Classic compactions");
    }
    if m.context_doc_chars > 0 {
        tracing::info!(
            chars = m.context_doc_chars,
            lines = m.context_doc_lines,
            est_tokens = m.context_doc_est_tokens,
            "RLM context doc"
        );
    }
    if m.graph_nodes > 0 {
        tracing::info!(
            nodes = m.graph_nodes,
            active = m.graph_active_nodes,
            edges = m.graph_edges,
            symlinks = m.graph_symlinks,
            tokens_saved = m.graph_tokens_saved,
            vector_entries = m.graph_vector_entries,
            vocab_size = m.graph_vocab_size,
            "SemanticGraph stats"
        );
    }
}

/// Generate a markdown comparison report
pub fn generate_report(runs: &[BenchMetrics]) -> String {
    let mut md = String::new();

    md.push_str("# Context Strategy Benchmark: RLM vs SemanticGraph vs Classic\n\n");
    md.push_str(&format!("**Date:** {}\n", chrono_date()));
    md.push_str(&format!("**Task:** {}\n\n", runs.first().map(|r| r.task.as_str()).unwrap_or("N/A")));

    // Summary table
    md.push_str("## Results Summary\n\n");
    md.push_str("| Metric | ");
    for r in runs {
        md.push_str(&format!("{} | ", r.strategy));
    }
    md.push_str("\n|--------|");
    for _ in runs {
        md.push_str("--------|");
    }
    md.push('\n');

    // Duration
    md.push_str("| Duration | ");
    for r in runs {
        md.push_str(&format!("{:.1}s | ", r.total_duration.as_secs_f64()));
    }
    md.push('\n');

    // Turns
    md.push_str("| Turns | ");
    for r in runs {
        md.push_str(&format!("{} | ", r.total_turns));
    }
    md.push('\n');

    // Completed
    md.push_str("| Completed | ");
    for r in runs {
        md.push_str(&format!("{} | ", if r.completed { "yes" } else { "no" }));
    }
    md.push('\n');

    // Total tokens
    md.push_str("| Total tokens | ");
    for r in runs {
        md.push_str(&format!("{} | ", r.total_tokens));
    }
    md.push('\n');

    // Input tokens
    md.push_str("| Input tokens | ");
    for r in runs {
        md.push_str(&format!("{} | ", r.total_input_tokens));
    }
    md.push('\n');

    // Output tokens
    md.push_str("| Output tokens | ");
    for r in runs {
        md.push_str(&format!("{} | ", r.total_output_tokens));
    }
    md.push('\n');

    // Tool calls
    md.push_str("| Tool calls | ");
    for r in runs {
        md.push_str(&format!("{} | ", r.total_tool_calls));
    }
    md.push('\n');

    // Tool errors
    md.push_str("| Tool errors | ");
    for r in runs {
        md.push_str(&format!("{} | ", r.tool_errors));
    }
    md.push('\n');

    // Compactions
    md.push_str("| Compactions | ");
    for r in runs {
        md.push_str(&format!("{} | ", r.compactions));
    }
    md.push('\n');

    md.push_str("\n## Strategy-Specific Metrics\n\n");

    // RLM metrics
    for r in runs.iter().filter(|r| r.strategy == "rlm") {
        md.push_str("### RLM (Recursive Lexical Memory)\n\n");
        md.push_str(&format!("- **Context document size:** {} chars, {} lines\n", r.context_doc_chars, r.context_doc_lines));
        md.push_str(&format!("- **Estimated full-history tokens:** ~{}\n", r.context_doc_est_tokens));
        md.push_str(&format!("- **Context window used:** 60% of {} = {} tokens\n", 128_000, (128_000.0 * 0.6) as usize));
        md.push_str(&format!("- **Compression ratio:** full history ~{} tokens, window ~{} tokens\n\n",
            r.context_doc_est_tokens, (128_000.0 * 0.6) as usize));
    }

    // SemanticGraph metrics
    for r in runs.iter().filter(|r| r.strategy == "semantic_graph") {
        md.push_str("### SemanticGraph (Knowledge Graph + TF-IDF)\n\n");
        md.push_str(&format!("- **Graph nodes:** {} total ({} active)\n", r.graph_nodes, r.graph_active_nodes));
        md.push_str(&format!("- **Graph edges:** {}\n", r.graph_edges));
        md.push_str(&format!("- **Vector store entries:** {}\n", r.graph_vector_entries));
        md.push_str(&format!("- **Vocabulary size:** {}\n", r.graph_vocab_size));
        md.push_str(&format!("- **Symlinks:** {} ({} tokens saved)\n\n", r.graph_symlinks, r.graph_tokens_saved));
    }

    // Classic metrics
    for r in runs.iter().filter(|r| r.strategy == "classic") {
        md.push_str("### Classic (Truncation + Compaction)\n\n");
        md.push_str(&format!("- **Compactions triggered:** {}\n", r.compactions));
        if !r.tokens_before_compaction.is_empty() {
            for (i, (before, after)) in r.tokens_before_compaction.iter()
                .zip(r.tokens_after_compaction.iter()).enumerate()
            {
                md.push_str(&format!("  - Compaction {}: {} → {} tokens (lost {})\n",
                    i + 1, before, after, before.saturating_sub(*after)));
            }
        }
        md.push('\n');
    }

    // Tool usage breakdown
    md.push_str("## Tool Usage Breakdown\n\n");
    md.push_str("| Tool | ");
    for r in runs {
        md.push_str(&format!("{} | ", r.strategy));
    }
    md.push_str("\n|------|");
    for _ in runs {
        md.push_str("------|");
    }
    md.push('\n');

    // Collect all tool names
    let mut all_tools: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for r in runs {
        for (name, _) in &r.tool_calls {
            all_tools.insert(name.clone());
        }
    }

    for tool in &all_tools {
        md.push_str(&format!("| {tool} | "));
        for r in runs {
            let count = r.tool_calls.iter()
                .find(|(n, _)| n == tool)
                .map(|(_, c)| *c)
                .unwrap_or(0);
            md.push_str(&format!("{count} | "));
        }
        md.push('\n');
    }

    // Turn latency
    md.push_str("\n## Turn Latency\n\n");
    for r in runs {
        md.push_str(&format!("### {}\n\n", r.strategy));
        if r.per_turn_latency.is_empty() {
            md.push_str("No turn data captured.\n\n");
        } else {
            let avg: f64 = r.per_turn_latency.iter().map(|d| d.as_secs_f64()).sum::<f64>()
                / r.per_turn_latency.len() as f64;
            let max = r.per_turn_latency.iter().map(|d| d.as_secs_f64()).fold(0.0f64, f64::max);
            let min = r.per_turn_latency.iter().map(|d| d.as_secs_f64()).fold(f64::MAX, f64::min);
            md.push_str(&format!("- Avg: {avg:.2}s, Min: {min:.2}s, Max: {max:.2}s\n"));
            md.push_str("- Per turn: ");
            for (i, d) in r.per_turn_latency.iter().enumerate() {
                if i > 0 { md.push_str(", "); }
                md.push_str(&format!("{:.1}s", d.as_secs_f64()));
            }
            md.push_str("\n\n");
        }
    }

    // Analysis
    md.push_str("## Analysis\n\n");
    md.push_str("### Token Efficiency\n\n");

    if runs.len() >= 2 {
        let rlm = runs.iter().find(|r| r.strategy == "rlm");
        let sg = runs.iter().find(|r| r.strategy == "semantic_graph");
        let classic = runs.iter().find(|r| r.strategy == "classic");

        if let (Some(rlm), Some(sg)) = (rlm, sg) {
            if rlm.total_tokens > 0 && sg.total_tokens > 0 {
                let ratio = rlm.total_tokens as f64 / sg.total_tokens as f64;
                md.push_str(&format!("- RLM used {:.1}x tokens compared to SemanticGraph\n", ratio));
            }
            if rlm.total_turns > 0 && sg.total_turns > 0 {
                md.push_str(&format!("- RLM took {} turns vs SemanticGraph's {} turns\n",
                    rlm.total_turns, sg.total_turns));
            }
        }

        if let Some(classic) = classic {
            md.push_str(&format!("- Classic triggered {} compaction(s), losing context\n", classic.compactions));
        }
    }

    md.push_str("\n### Design Tradeoffs\n\n");
    md.push_str("| Dimension | RLM | SemanticGraph | Classic |\n");
    md.push_str("|-----------|-----|---------------|----------|\n");
    md.push_str("| Context preservation | Full history as document | Full graph, nothing deleted | Truncated/compacted |\n");
    md.push_str("| Token overhead per turn | Low (recent window only) | Variable (retrieval-based) | Grows until compaction |\n");
    md.push_str("| Information loss | None | None | Compaction loses detail |\n");
    md.push_str("| Retrieval quality | Sequential (recent turns) | Semantic (relevance-based) | N/A (linear) |\n");
    md.push_str("| Best for | Long sequential tasks | Multi-topic/jumping tasks | Short tasks (<context window) |\n");

    md.push_str("\n---\n\n");
    md.push_str("*Generated by `amai --benchmark`*\n");

    md
}

fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max { s } else { &s[..max] }
}

fn chrono_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn chrono_date() -> String {
    // Simple date without chrono dependency
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Unix timestamp to approximate date
    let days = secs / 86400;
    let years = 1970 + (days / 365); // rough
    let remaining_days = days % 365;
    let month = remaining_days / 30 + 1;
    let day = remaining_days % 30 + 1;
    format!("{years}-{month:02}-{day:02} (approximate)")
}

/// Run a synthetic (no-LLM) benchmark comparing context management overhead.
/// This measures serialization, graph construction, and retrieval costs
/// using simulated conversation data from a realistic codebase analysis task.
pub fn run_synthetic_benchmark() -> String {
    use soul_core::rlm::RlmEngine;
    use soul_core::semantic_recursion::{RetrievalQuery, SemanticContextEngine};
    use std::time::Instant;

    // Simulate a 20-turn conversation analyzing amai-infra
    let conversation = build_synthetic_conversation();

    let mut report = String::new();
    report.push_str("# Synthetic Context Strategy Benchmark\n\n");
    report.push_str("**Mode:** Deterministic (no LLM calls)\n");
    report.push_str("**Conversation:** 20-turn simulated codebase analysis\n\n");

    // ─── RLM Benchmark ───────────────────────────────────────────
    let start = Instant::now();
    let doc = RlmEngine::serialize_conversation(&conversation);
    let serialize_time = start.elapsed();

    let start = Instant::now();
    let metadata = RlmEngine::context_metadata(&doc);
    let metadata_time = start.elapsed();

    let start = Instant::now();
    let (window, _meta) = RlmEngine::build_context_window(&conversation, 76_800);
    let window_time = start.elapsed();

    report.push_str("## RLM (Recursive Lexical Memory)\n\n");
    report.push_str(&format!("| Metric | Value |\n"));
    report.push_str(&format!("|--------|-------|\n"));
    report.push_str(&format!("| Serialize time | {:?} |\n", serialize_time));
    report.push_str(&format!("| Metadata time | {:?} |\n", metadata_time));
    report.push_str(&format!("| Window build time | {:?} |\n", window_time));
    report.push_str(&format!("| Document chars | {} |\n", doc.len()));
    report.push_str(&format!("| Document lines | {} |\n", doc.lines().count()));
    report.push_str(&format!("| Est. document tokens | ~{} |\n", doc.len() / 4));
    report.push_str(&format!("| Window messages | {} of {} |\n", window.len(), conversation.len()));
    report.push_str(&format!("| Metadata | {} |\n", metadata));
    report.push_str("\n");

    // ─── SemanticGraph Benchmark ─────────────────────────────────
    let start = Instant::now();
    let mut engine = SemanticContextEngine::new();
    let mut last_user_node = 0u64;

    for msg in &conversation {
        match msg.role {
            Role::User => {
                last_user_node = engine.ingest_user_request(&msg.text_content());
            }
            Role::Assistant => {
                let text = msg.text_content();
                if !text.is_empty() {
                    engine.ingest_llm_response(&text, last_user_node, Some("gemini-2.5-flash"));
                }
                for block in &msg.content {
                    if let ContentBlock::ToolCall { name, arguments, .. } = block {
                        let args_str = arguments.to_string();
                        engine.ingest_tool_interaction(name, &args_str, "(result)", last_user_node + 1);
                    }
                }
            }
            _ => {}
        }
    }
    let ingest_time = start.elapsed();

    let start = Instant::now();
    let stats = engine.stats();
    let stats_time = start.elapsed();

    let start = Instant::now();
    let retrieval = engine.retrieve(&RetrievalQuery {
        text: "service ports and test counts".into(),
        max_tokens: 76_800,
        max_results: 50,
        include_graph_neighbors: true,
    });
    let retrieval_time = start.elapsed();

    report.push_str("## SemanticGraph (Knowledge Graph + TF-IDF)\n\n");
    report.push_str(&format!("| Metric | Value |\n"));
    report.push_str(&format!("|--------|-------|\n"));
    report.push_str(&format!("| Ingest time (all turns) | {:?} |\n", ingest_time));
    report.push_str(&format!("| Stats time | {:?} |\n", stats_time));
    report.push_str(&format!("| Retrieval time | {:?} |\n", retrieval_time));
    report.push_str(&format!("| Total nodes | {} |\n", stats.total_nodes));
    report.push_str(&format!("| Active nodes | {} |\n", stats.active_nodes));
    report.push_str(&format!("| Total edges | {} |\n", stats.total_edges));
    report.push_str(&format!("| Vector entries | {} |\n", stats.vector_entries));
    report.push_str(&format!("| Vocab size | {} |\n", stats.vocab_size));
    report.push_str(&format!("| Symlinks | {} |\n", stats.symlink_count));
    report.push_str(&format!("| Retrieved messages | {} |\n", retrieval.messages.len()));
    report.push_str(&format!("| Retrieved tokens | {} |\n", retrieval.total_tokens));
    if !retrieval.relevance_scores.is_empty() {
        let avg_score: f32 = retrieval.relevance_scores.iter().sum::<f32>()
            / retrieval.relevance_scores.len() as f32;
        let max_score = retrieval.relevance_scores.iter().fold(0.0f32, |a, &b| a.max(b));
        report.push_str(&format!("| Avg relevance | {:.3} |\n", avg_score));
        report.push_str(&format!("| Max relevance | {:.3} |\n", max_score));
    }
    report.push_str("\n");

    // ─── Classic Baseline ────────────────────────────────────────
    let total_tokens: usize = conversation.iter().map(|m| m.estimate_tokens()).sum();

    report.push_str("## Classic (Baseline)\n\n");
    report.push_str(&format!("| Metric | Value |\n"));
    report.push_str(&format!("|--------|-------|\n"));
    report.push_str(&format!("| Total message tokens | ~{} |\n", total_tokens));
    report.push_str(&format!("| Messages count | {} |\n", conversation.len()));
    report.push_str(&format!("| Would compact at | ~{} tokens (85% of 128K) |\n", (128_000.0 * 0.85) as usize));
    report.push_str(&format!("| Compaction needed | {} |\n",
        if total_tokens > (128_000.0 * 0.85) as usize { "yes" } else { "no" }
    ));
    report.push_str("\n");

    // ─── Comparison ──────────────────────────────────────────────
    report.push_str("## Comparison\n\n");
    report.push_str("| Dimension | RLM | SemanticGraph | Classic |\n");
    report.push_str("|-----------|-----|---------------|----------|\n");
    report.push_str(&format!("| Setup overhead | {:?} | {:?} | 0 |\n", serialize_time, ingest_time));
    report.push_str(&format!("| Per-turn overhead | {:?} | {:?} | 0 |\n", window_time, retrieval_time));
    report.push_str(&format!("| History preserved | 100% ({} chars) | 100% ({} nodes) | Until compaction |\n",
        doc.len(), stats.total_nodes));
    report.push_str(&format!("| Window size | {} msgs | {} msgs | {} msgs |\n",
        window.len(), retrieval.messages.len(), conversation.len()));
    report.push_str(&format!("| Memory overhead | {} bytes | {} nodes + {} edges | 0 |\n",
        doc.len(), stats.total_nodes, stats.total_edges));
    report.push_str("\n");

    report.push_str("## Verdict\n\n");
    report.push_str("- **RLM**: Minimal overhead, preserves everything, simple sequential window. ");
    report.push_str("Best when the agent works sequentially and recent context is most important.\n");
    report.push_str("- **SemanticGraph**: Higher overhead but enables semantic retrieval across the full history. ");
    report.push_str("Best when the agent needs to reference distant context by topic.\n");
    report.push_str("- **Classic**: Zero overhead until compaction, then loses information permanently. ");
    report.push_str("Only suitable for short conversations that never exceed the context window.\n");

    report
}

/// Build a realistic 20-turn simulated conversation
fn build_synthetic_conversation() -> Vec<Message> {
    let mut msgs = Vec::new();

    // Helper: assistant message with a tool call
    let assistant_tc = |text: &str, tool: &str, id: &str, args: &str| -> Message {
        let args_val: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
        Message::new(
            Role::Assistant,
            vec![
                ContentBlock::text(text),
                ContentBlock::tool_call(id, tool, args_val),
            ],
        )
    };

    // Turn 1: User asks to analyze the codebase
    msgs.push(Message::user("Analyze the amai-infra monorepo. List all services, their ports, programming languages, and test counts. Identify the key architectural patterns."));

    // Turn 1: Assistant uses ls
    msgs.push(assistant_tc("Let me start by exploring the repository structure.", "ls", "tc1", r#"{"path": "/Users/feral/amai-infra"}"#));
    msgs.push(Message::tool_result("tc1", "services/  soul/  amai/  research/  .moon/  docker-compose.yml  Cargo.toml  package.json", false));

    // Turn 2: List services
    msgs.push(assistant_tc("I'll list the services directory.", "ls", "tc2", r#"{"path": "/Users/feral/amai-infra/services"}"#));
    msgs.push(Message::tool_result("tc2", "id-service/  mock-llm-service/  agent-jail/  trading-service/  shepherd-service/  shadow-service/  soullog/", false));

    // Turn 3: Read id-service
    msgs.push(assistant_tc("Let me check the id-service first.", "read", "tc3", r#"{"path": "/Users/feral/amai-infra/services/id-service/Cargo.toml"}"#));
    msgs.push(Message::tool_result("tc3", "[package]\nname = \"id-service\"\nversion = \"0.1.0\"\n\n[dependencies]\naxum = \"0.7\"\ntokio = { version = \"1\", features = [\"full\"] }\nserde = { version = \"1\", features = [\"derive\"] }\nthiserror = \"2\"\ndashmap = \"6\"", false));

    // Turn 4: id-service tests
    msgs.push(assistant_tc("Running id-service tests.", "bash", "tc4", r#"{"command": "cargo test"}"#));
    msgs.push(Message::tool_result("tc4", "test result: ok. 32 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.15s", false));

    // Turn 5: Read mock-llm
    msgs.push(assistant_tc("Checking mock-llm-service.", "read", "tc5", r#"{"path": "/Users/feral/amai-infra/services/mock-llm-service/Cargo.toml"}"#));
    msgs.push(Message::tool_result("tc5", "[package]\nname = \"mock-llm-service\"\nversion = \"0.2.0\"\n\n[dependencies]\naxum = \"0.7\"\ntokio = { version = \"1\", features = [\"full\"] }\nreqwest = \"0.12\"\nsha2 = \"0.10\"", false));

    // Turn 6: mock-llm tests
    msgs.push(assistant_tc("Running mock-llm tests.", "bash", "tc6", r#"{"command": "cargo test"}"#));
    msgs.push(Message::tool_result("tc6", "test result: ok. 80 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.82s", false));

    // Turn 7: agent-jail
    msgs.push(assistant_tc("Checking agent-jail.", "bash", "tc7", r#"{"command": "cargo test"}"#));
    msgs.push(Message::tool_result("tc7", "test result: ok. 148 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; finished in 2.31s", false));

    // Turn 8: trading-service
    msgs.push(assistant_tc("Checking TypeScript services. Starting with trading-service.", "bash", "tc8", r#"{"command": "npx vitest run"}"#));
    msgs.push(Message::tool_result("tc8", " Tests  31 passed (6 test files)\n Duration  1.23s\n\nTest Files  6 passed (6)\n     Tests  31 passed (31)", false));

    // Turn 9: shepherd-service
    msgs.push(assistant_tc("Checking shepherd-service.", "bash", "tc9", r#"{"command": "npx vitest run"}"#));
    msgs.push(Message::tool_result("tc9", " Tests  35 passed (4 test files)\n Duration  0.89s\n\nTest Files  4 passed (4)\n     Tests  35 passed (35)", false));

    // Turn 10: shadow-service
    msgs.push(assistant_tc("Checking shadow-service.", "bash", "tc10", r#"{"command": "npx vitest run"}"#));
    msgs.push(Message::tool_result("tc10", "Test Files  6 passed (6)\n     Tests  75 passed (75)\n Duration  1.45s", false));

    // Turn 11: soullog
    msgs.push(assistant_tc("And soullog.", "bash", "tc11", r#"{"command": "npx vitest run"}"#));
    msgs.push(Message::tool_result("tc11", "Test Files  5 passed (5)\n     Tests  33 passed (33)\n Duration  0.67s", false));

    // Turn 12: soul-core
    msgs.push(assistant_tc("Now checking the soul crates.", "bash", "tc12", r#"{"command": "cargo test"}"#));
    msgs.push(Message::tool_result("tc12", "running 692 tests\n...\ntest result: ok. 692 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.89s", false));

    // Turn 13: Grep for ports
    msgs.push(assistant_tc("Finding port assignments.", "grep", "tc13", r#"{"pattern": "port.*8080"}"#));
    msgs.push(Message::tool_result("tc13", "services/id-service/src/config.rs:    port: 8080\nservices/mock-llm-service/src/config.rs:    port: 8081\nservices/agent-jail/src/config.rs:    port: 8082\nservices/trading-service/src/index.ts:    port: 8083\nservices/shepherd-service/src/index.ts:    port: 8084\nservices/shadow-service/src/index.ts:    port: 8085\nservices/soullog/src/index.ts:    port: 8086", false));

    // Turn 14: Moon config
    msgs.push(assistant_tc("Checking monorepo orchestration.", "read", "tc14", r#"{"path": ".moon/workspace.yml"}"#));
    msgs.push(Message::tool_result("tc14", "projects:\n  - services/*\n  - soul/*\n  - amai/*", false));

    // Turn 15: Final summary
    let summary = "## AMAI Infrastructure Analysis\n\n\
        ### Services\n\n\
        | Service | Port | Language | Tests |\n\
        |---------|------|----------|-------|\n\
        | id-service | 8080 | Rust (Axum 0.7) | 32 |\n\
        | mock-llm-service | 8081 | Rust (Axum 0.7) | 80 |\n\
        | agent-jail | 8082 | Rust (Axum 0.7) | 152 (4 ignored) |\n\
        | trading-service | 8083 | TypeScript (Express 5) | 31 |\n\
        | shepherd-service | 8084 | TypeScript (Express 5) | 35 |\n\
        | shadow-service | 8085 | TypeScript (Express 5) | 75 |\n\
        | soullog | 8086 | TypeScript (Express 5) | 33 |\n\n\
        ### Core Crates\n\n\
        | Crate | Tests | Purpose |\n\
        |-------|-------|---------|\n\
        | soul-core | 692 | Async agentic runtime |\n\
        | soul-coder | ~20 | Coding tools (read/write/edit/bash) |\n\n\
        ### Key Patterns\n\
        - Rust services: Axum 0.7 + Tokio + DashMap + thiserror\n\
        - TS services: Express 5 + Zod + Vitest + Ed25519 auth\n\
        - Monorepo: Moon orchestration across 7 services + soul crates\n\
        - Auth: Ed25519 signed request envelopes verified by id-service\n\
        - Storage: filesystem-based (JSON + JSONL), no database dependency";

    msgs.push(Message::assistant(summary));

    msgs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_metrics_new() {
        let m = BenchMetrics::new("rlm", "analyze code");
        assert_eq!(m.strategy, "rlm");
        assert_eq!(m.task, "analyze code");
        assert_eq!(m.total_turns, 0);
        assert!(!m.completed);
    }

    #[test]
    fn generate_report_empty() {
        let report = generate_report(&[]);
        assert!(report.contains("Context Strategy Benchmark"));
    }

    #[test]
    fn generate_report_single_run() {
        let m = BenchMetrics {
            strategy: "rlm".into(),
            task: "test task".into(),
            total_duration: Duration::from_secs(10),
            per_turn_latency: vec![Duration::from_secs(3), Duration::from_secs(7)],
            total_turns: 2,
            completed: true,
            final_text: "done".into(),
            total_input_tokens: 1000,
            total_output_tokens: 500,
            total_tokens: 1500,
            tool_calls: vec![("read".into(), 3), ("bash".into(), 1)],
            total_tool_calls: 4,
            tool_errors: 0,
            compactions: 0,
            tokens_before_compaction: vec![],
            tokens_after_compaction: vec![],
            context_doc_chars: 5000,
            context_doc_lines: 100,
            context_doc_est_tokens: 1250,
            graph_nodes: 0,
            graph_active_nodes: 0,
            graph_edges: 0,
            graph_symlinks: 0,
            graph_tokens_saved: 0,
            graph_vector_entries: 0,
            graph_vocab_size: 0,
        };

        let report = generate_report(&[m]);
        assert!(report.contains("rlm"));
        assert!(report.contains("10.0s"));
        assert!(report.contains("1500"));
        assert!(report.contains("Context document size"));
    }

    #[test]
    fn synthetic_benchmark_runs() {
        let report = run_synthetic_benchmark();
        assert!(report.contains("Synthetic Context Strategy Benchmark"));
        assert!(report.contains("RLM"));
        assert!(report.contains("SemanticGraph"));
        assert!(report.contains("Classic"));
        assert!(report.contains("Comparison"));
        assert!(report.contains("Verdict"));
        // Verify actual data was computed
        assert!(report.contains("Document chars"));
        assert!(report.contains("Total nodes"));
        // Print report for inspection when running with --nocapture
        eprintln!("\n{report}");
    }

    #[test]
    fn synthetic_conversation_realistic_size() {
        let conv = build_synthetic_conversation();
        assert!(conv.len() >= 25, "Should have at least 25 messages (user + assistant + tool results)");
        // Check first message is user
        assert_eq!(conv[0].role, Role::User);
        // Check last message is assistant summary
        assert_eq!(conv.last().unwrap().role, Role::Assistant);
        // Check tool calls exist
        let tool_calls: usize = conv.iter()
            .filter(|m| m.has_tool_calls())
            .count();
        assert!(tool_calls >= 10, "Should have at least 10 tool call turns");
    }

    #[test]
    fn generate_report_comparison() {
        let rlm = BenchMetrics {
            strategy: "rlm".into(),
            task: "test".into(),
            total_duration: Duration::from_secs(10),
            per_turn_latency: vec![],
            total_turns: 5,
            completed: true,
            final_text: "done".into(),
            total_input_tokens: 2000,
            total_output_tokens: 1000,
            total_tokens: 3000,
            tool_calls: vec![],
            total_tool_calls: 10,
            tool_errors: 0,
            compactions: 0,
            tokens_before_compaction: vec![],
            tokens_after_compaction: vec![],
            context_doc_chars: 10000,
            context_doc_lines: 200,
            context_doc_est_tokens: 2500,
            graph_nodes: 0,
            graph_active_nodes: 0,
            graph_edges: 0,
            graph_symlinks: 0,
            graph_tokens_saved: 0,
            graph_vector_entries: 0,
            graph_vocab_size: 0,
        };

        let sg = BenchMetrics {
            strategy: "semantic_graph".into(),
            task: "test".into(),
            total_duration: Duration::from_secs(12),
            per_turn_latency: vec![],
            total_turns: 4,
            completed: true,
            final_text: "done".into(),
            total_input_tokens: 1500,
            total_output_tokens: 800,
            total_tokens: 2300,
            tool_calls: vec![],
            total_tool_calls: 8,
            tool_errors: 0,
            compactions: 0,
            tokens_before_compaction: vec![],
            tokens_after_compaction: vec![],
            context_doc_chars: 0,
            context_doc_lines: 0,
            context_doc_est_tokens: 0,
            graph_nodes: 12,
            graph_active_nodes: 12,
            graph_edges: 18,
            graph_symlinks: 3,
            graph_tokens_saved: 500,
            graph_vector_entries: 12,
            graph_vocab_size: 80,
        };

        let report = generate_report(&[rlm, sg]);
        assert!(report.contains("RLM used"));
        assert!(report.contains("SemanticGraph"));
        assert!(report.contains("Graph nodes"));
    }
}

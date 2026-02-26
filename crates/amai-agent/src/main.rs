mod agent_helpers;
mod bench;
mod config;
mod delegate;
mod google;
mod identity;
mod install_skill;
mod plan_execute;
mod prompt;
mod shepherd_gateway;
mod soullog_client;
mod state;
mod wiring;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use soul_core::agent::{AgentLoop, RunOptions};
use soul_core::gateway::{Gateway, GatewayEvent, GatewayMessage};
use soul_core::skill::{
    parser::parse_skill, LuaSkillExecutor, ShellSkillExecutor, SkillExecution, SkillToolBridge,
};
use soul_core::tool::ToolRegistry;
use soul_core::types::{AgentConfig, AgentEvent, ContextStrategy, Message};
use soul_core::vfs::NativeFs;
use soul_core::vexec::NativeExecutor;
use soul_gateways::telegram::TelegramGateway;
use amai_tools::{ContractsTool, ShepherdTool, agent_tools_vec};

#[derive(Parser)]
#[command(name = "amai", about = "AMAI autonomous coding agent")]
struct Cli {
    /// Task to execute (prompt)
    #[arg(trailing_var_arg = true)]
    task: Vec<String>,

    /// Config file path (default: amai-agent.toml)
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Working directory
    #[arg(short = 'd', long)]
    cwd: Option<String>,

    /// Maximum agent turns
    #[arg(long)]
    max_turns: Option<usize>,

    /// Print mode — single response, no interactive loop
    #[arg(long)]
    print: bool,

    /// Benchmark mode — run all three context strategies and compare
    #[arg(long)]
    benchmark: bool,

    /// Synthetic benchmark — no LLM calls, measures context management overhead
    #[arg(long)]
    synthetic: bool,

    /// Output file for benchmark report (default: stdout)
    #[arg(long)]
    report: Option<PathBuf>,

    /// Timeout in minutes (wall-clock deadline)
    #[arg(long)]
    timeout: Option<u64>,

    /// Context file(s) to inject as initial context before the task
    #[arg(long = "context-file", num_args = 1..)]
    context_files: Vec<PathBuf>,

    /// Run in Telegram gateway mode (interactive, listens for messages)
    #[arg(long)]
    telegram: bool,

    /// Resume a previous session (use 'latest' for most recent)
    #[arg(long)]
    resume: Option<String>,

    /// Run in Shepherd gateway mode (managed by shepherd-service over WebSocket)
    #[arg(long)]
    shepherd: Option<String>,

    /// Run as supervisor (auto-restart loop with optional self-compile)
    #[arg(long)]
    supervisor: bool,

    /// Agent purpose — selects system prompt: "code" (default), "research"
    #[arg(long)]
    purpose: Option<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("amai=info".parse().unwrap()))
        .compact()
        .init();

    let cli = Cli::parse();

    // Supervisor mode: fork-exec loop that restarts the agent
    if cli.supervisor {
        run_supervisor().await;
        return;
    }

    // Load config
    let toml_config = if let Some(ref path) = cli.config {
        config::AgentToml::load(path).unwrap_or_else(|e| {
            tracing::error!(error = %e, "Config error");
            std::process::exit(1);
        })
    } else {
        config::AgentToml::load_default().unwrap_or_else(|e| {
            tracing::error!(error = %e, "Config error");
            std::process::exit(1);
        })
    };

    // Build balanced provider
    let balanced = if toml_config.providers.is_empty() {
        tracing::info!("No providers configured — using local Ollama");
        wiring::build_default_ollama()
    } else {
        wiring::build_balanced(&toml_config).unwrap_or_else(|e| {
            tracing::error!(error = %e, "Provider setup error");
            std::process::exit(1);
        })
    };

    let provider = Arc::new(balanced);
    let status = provider.status();
    tracing::info!(
        slots = status.total_slots,
        available = status.available_slots,
        "Balanced provider ready"
    );

    // Working directory
    let cwd = cli
        .cwd
        .or_else(|| {
            if toml_config.agent.cwd != "." {
                Some(toml_config.agent.cwd.clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap().to_string_lossy().into());

    // Synthetic benchmark mode (no LLM calls) — doesn't need a task
    if cli.synthetic {
        let report = bench::run_synthetic_benchmark();
        if let Some(ref path) = cli.report {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(path, &report).unwrap_or_else(|e| {
                tracing::error!(path = %path.display(), error = %e, "Failed to write report");
                std::process::exit(1);
            });
            tracing::info!(path = %path.display(), "Synthetic benchmark report written");
        } else {
            print!("{report}");
        }
        return;
    }

    // Telegram gateway mode — interactive loop
    if cli.telegram || toml_config.telegram.is_some() {
        run_telegram_mode(provider, &toml_config, &cwd).await;
        return;
    }

    // Shepherd gateway mode — managed by shepherd-service over WebSocket
    let shepherd_url = cli.shepherd.or_else(|| {
        toml_config.shepherd.as_ref().and_then(|s| s.url.clone())
    });
    if let Some(ref url) = shepherd_url {
        let max_turns = cli.max_turns.unwrap_or(toml_config.agent.max_turns);
        let heartbeat_secs = toml_config
            .shepherd
            .as_ref()
            .and_then(|s| s.heartbeat_secs)
            .unwrap_or(15);
        run_shepherd_mode(provider, &toml_config, &cwd, url, max_turns, heartbeat_secs).await;
        return;
    }

    // Get task from args
    let task = if cli.task.is_empty() {
        if cli.benchmark {
            "Analyze this codebase. List all services, their ports, programming languages, \
             and test counts. Identify the key architectural patterns. Summarize in a structured report."
                .to_string()
        } else {
            tracing::error!("No task provided");
            eprintln!("Usage: amai <task>");
            eprintln!("       amai --telegram                             (Telegram gateway mode)");
            eprintln!("       amai --shepherd ws://host:8084/ws/sessions/ID  (Shepherd managed mode)");
            eprintln!("       amai --supervisor --telegram                 (supervised auto-restart)");
            eprintln!("       amai --benchmark [task]");
            eprintln!("       amai --synthetic");
            eprintln!("Example: amai \"fix the failing test in src/lib.rs\"");
            std::process::exit(1);
        }
    } else {
        cli.task.join(" ")
    };

    // Benchmark mode
    if cli.benchmark {
        let max_turns = cli.max_turns.unwrap_or(15);
        tracing::info!(
            task = %truncate(&task, 100),
            cwd = %cwd,
            max_turns,
            "Starting context strategy benchmark"
        );

        let strategies = [
            ContextStrategy::Rlm,
            ContextStrategy::SemanticGraph,
            ContextStrategy::Classic,
        ];

        let mut results = Vec::new();
        for strategy in strategies {
            let metrics = bench::run_benchmark(
                provider.clone(),
                strategy,
                &task,
                max_turns,
                &cwd,
            )
            .await;
            results.push(metrics);
        }

        let report = bench::generate_report(&results);

        if let Some(ref path) = cli.report {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(path, &report).unwrap_or_else(|e| {
                tracing::error!(path = %path.display(), error = %e, "Failed to write benchmark report");
                std::process::exit(1);
            });
            tracing::info!(path = %path.display(), "Benchmark report written");
        } else {
            print!("{report}");
        }

        return;
    }

    // Normal agent mode — single task execution
    let max_turns = cli
        .max_turns
        .unwrap_or(toml_config.agent.max_turns);

    let result = run_single_task(
        provider,
        &toml_config,
        &cwd,
        &task,
        max_turns,
        &cli.context_files,
        cli.timeout,
        cli.resume.as_deref(),
        cli.purpose.as_deref(),
    )
    .await;

    match result {
        Ok(_) => {}
        Err(e) => {
            tracing::error!(error = %e, "Agent error");
            std::process::exit(1);
        }
    }
}

// ─── Disk Logging ────────────────────────────────────────────────────────────

struct DiskLogger {
    event_file: std::sync::Mutex<Option<std::fs::File>>,
    session_id: String,
}

impl DiskLogger {
    fn new(log_dir: &str, session_id: &str) -> Self {
        let dir = std::path::Path::new(log_dir);
        std::fs::create_dir_all(dir).ok();

        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let event_path = dir.join(format!("{session_id}_{timestamp}_events.jsonl"));

        let file = std::fs::File::create(&event_path)
            .map_err(|e| tracing::warn!(path = %event_path.display(), error = %e, "Failed to create event log"))
            .ok();

        if file.is_some() {
            tracing::info!(path = %event_path.display(), "Event log opened");
        }

        Self {
            event_file: std::sync::Mutex::new(file),
            session_id: session_id.to_string(),
        }
    }

    fn log_event(&self, event: &AgentEvent) {
        let mut lock = self.event_file.lock().unwrap();
        if let Some(ref mut file) = *lock {
            use std::io::Write;
            let record = serde_json::json!({
                "ts": chrono::Utc::now().to_rfc3339(),
                "session": self.session_id,
                "event": event,
            });
            if let Ok(line) = serde_json::to_string(&record) {
                let _ = writeln!(file, "{line}");
            }
        }
    }

    fn log_conversation(&self, log_dir: &str, messages: &[Message]) {
        let dir = std::path::Path::new(log_dir);
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let path = dir.join(format!("{}_{}_conversation.json", self.session_id, timestamp));
        if let Ok(json) = serde_json::to_string_pretty(messages) {
            let _ = std::fs::write(&path, json);
            tracing::info!(path = %path.display(), messages = messages.len(), "Conversation snapshot saved");
        }
    }
}

// ─── Telegram Gateway Mode ──────────────────────────────────────────────────

async fn run_telegram_mode(
    provider: Arc<soul_core::provider::balanced::BalancedProvider>,
    config: &config::AgentToml,
    cwd: &str,
) {
    let tg_config = match &config.telegram {
        Some(tg) => tg,
        None => {
            tracing::error!("Telegram mode requires [telegram] section in config");
            std::process::exit(1);
        }
    };

    let token = match tg_config.resolve_token() {
        Some(t) => t,
        None => {
            tracing::error!("Telegram token not found. Set token or token_env in [telegram] config.");
            std::process::exit(1);
        }
    };

    let gateway = TelegramGateway::new(&token)
        .with_allowed_users(tg_config.allowed_users.clone());

    // Start gateway
    let (gw_event_tx, mut gw_event_rx) = mpsc::unbounded_channel();
    gateway.start(gw_event_tx).await.unwrap_or_else(|e| {
        tracing::error!(error = %e, "Failed to start Telegram gateway");
        std::process::exit(1);
    });

    tracing::info!(
        bot = "amai_oracle_bot",
        allowed_users = ?tg_config.allowed_users,
        "Telegram gateway active"
    );

    // Send startup notification
    if let Some(ref chat_id) = tg_config.startup_chat_id {
        let startup_msg = format!(
            "AMAI Agent online.\n\
             CWD: {cwd}\n\
             Strategy: {}\n\
             Max turns: {}\n\
             Context: {}\n\
             Providers: {}\n\n\
             Ready to accept workload.",
            config.agent.strategy,
            config.agent.max_turns,
            config.agent.context_strategy,
            config.providers.keys().cloned().collect::<Vec<_>>().join(", "),
        );
        if let Err(e) = gateway.send(chat_id, GatewayMessage::text(&startup_msg)).await {
            tracing::warn!(error = %e, "Failed to send startup notification");
        } else {
            tracing::info!(chat_id, "Startup notification sent");
        }
    }

    // Message processing loop — each TG message becomes an agent task
    let max_turns = config.agent.max_turns;
    let log_config = &config.logging;
    let log_dir = log_config.dir.clone();

    loop {
        match gw_event_rx.recv().await {
            Some(GatewayEvent::MessageReceived {
                channel_id,
                sender,
                text,
                ..
            }) => {
                tracing::info!(sender = %sender, text = %truncate(&text, 100), "Received message");

                // Handle /reset command
                if text.trim() == "/reset" {
                    let session_id = format!("tg-{sender}");
                    tracing::info!(session_id = %session_id, "Session reset requested");
                    let mut fresh = state::SessionState::new(&session_id);
                    if let Err(e) = fresh.save(cwd) {
                        tracing::warn!(error = %e, "Failed to save reset state");
                    }
                    let _ = gateway
                        .send(&channel_id, GatewayMessage::text("Session reset."))
                        .await;
                    continue;
                }

                // Send "thinking" indicator
                let _ = gateway
                    .send(&channel_id, GatewayMessage::text("..."))
                    .await;

                // Stable session ID per sender (persistent across messages)
                let session_id = format!("tg-{sender}");

                // Load prior state for this sender
                let mut session_state = match state::SessionState::load(cwd, &session_id) {
                    Some(s) => {
                        tracing::info!(
                            session_id = %s.session_id,
                            prior_messages = s.messages.len(),
                            prior_turns = s.total_turns,
                            "Resumed session state"
                        );
                        s
                    }
                    None => {
                        tracing::info!(session_id = %session_id, "New session — no prior state");
                        state::SessionState::new(&session_id)
                    }
                };

                // Build initial_messages: prior history + new user message
                let mut initial_messages = session_state.messages.clone();
                let user_msg = Message::user(&text);
                initial_messages.push(user_msg.clone());

                // Set up disk logging
                let logger = Arc::new(DiskLogger::new(&log_dir, &session_id));

                // Run agent for this message
                let result = run_agent_task(
                    provider.clone(),
                    config,
                    cwd,
                    initial_messages,
                    max_turns,
                    logger.clone(),
                    None,
                    None,
                )
                .await;

                match result {
                    Ok(new_messages) => {
                        // Send back the final text response
                        let response_text = new_messages
                            .last()
                            .map(|m| m.text_content())
                            .unwrap_or_default();

                        let send_text = if response_text.is_empty() {
                            "(no text output — check tool results)".to_string()
                        } else {
                            response_text
                        };

                        tracing::info!(
                            chat_id = %channel_id,
                            text_len = send_text.len(),
                            "Sending response to Telegram"
                        );

                        match gateway
                            .send(&channel_id, GatewayMessage::text(&send_text))
                            .await
                        {
                            Ok(()) => {
                                tracing::info!(chat_id = %channel_id, "Response sent to Telegram");
                            }
                            Err(e) => {
                                tracing::error!(
                                    chat_id = %channel_id,
                                    error = %e,
                                    "Failed to send response to Telegram"
                                );
                            }
                        }

                        // Persist session state
                        let mut all_messages = session_state.messages.clone();
                        all_messages.push(user_msg);
                        all_messages.extend(new_messages.clone());
                        session_state.messages = all_messages.clone();
                        session_state.total_turns += new_messages.len();
                        if let Err(e) = session_state.save(cwd) {
                            tracing::warn!(error = %e, "Failed to save session state");
                        }

                        // Log full conversation
                        logger.log_conversation(&log_dir, &all_messages);
                    }
                    Err(e) => {
                        let error_msg = format!("Agent error: {e}");
                        tracing::error!(error = %e, "Agent task failed");
                        match gateway
                            .send(&channel_id, GatewayMessage::text(&error_msg))
                            .await
                        {
                            Ok(()) => {}
                            Err(send_err) => {
                                tracing::error!(error = %send_err, "Failed to send error to Telegram");
                            }
                        }
                    }
                }
            }
            Some(GatewayEvent::Error { source, message }) => {
                tracing::error!(source = %source, error = %message, "Gateway error");
            }
            None => {
                tracing::info!("Gateway channel closed, shutting down");
                break;
            }
        }
    }

    gateway.stop().await.ok();
}

// ─── Shepherd Gateway Mode ───────────────────────────────────────────────────

async fn run_shepherd_mode(
    provider: Arc<soul_core::provider::balanced::BalancedProvider>,
    config: &config::AgentToml,
    cwd: &str,
    url: &str,
    max_turns: usize,
    heartbeat_secs: u64,
) {
    // ── Identity: load or register with id-service ──
    let agent_identity = if let Some(ref id_config) = config.identity {
        let agent_name = id_config.name.clone().unwrap_or_else(|| {
            format!(
                "amai-agent-{}",
                hostname::get()
                    .map(|h| h.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "unknown".into())
            )
        });
        match identity::load_or_register(
            &id_config.id_service_url,
            &agent_name,
            &id_config.key_dir,
        )
        .await
        {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(error = %e, "Identity registration failed — proceeding without identity");
                None
            }
        }
    } else {
        tracing::info!("No [identity] config — running without identity");
        None
    };

    let identity_id = agent_identity.as_ref().map(|id| id.identity_id.clone());

    let gateway = shepherd_gateway::ShepherdGateway::new(url, cwd, max_turns)
        .with_heartbeat_secs(heartbeat_secs)
        .with_identity(agent_identity.as_ref());

    let session_id = gateway.session_id().to_string();
    let log_dir = config.logging.dir.clone();

    // ── Soullog: create centralized logging client ──
    let soullog = if let Some(ref sl_config) = config.soullog {
        let client = soullog_client::SoullogClient::new(
            &sl_config.url,
            &sl_config.service,
            identity_id.clone(),
            Some(session_id.clone()),
            sl_config.batch_size,
            sl_config.flush_interval_ms,
        );
        // Log agent startup
        client.log_custom(
            "soullog.shepherd.sessions",
            "AgentStartup",
            serde_json::json!({
                "session_id": session_id,
                "identity_id": identity_id,
                "cwd": cwd,
                "max_turns": max_turns,
                "url": url,
            }),
        );
        Some(Arc::new(client))
    } else {
        tracing::info!("No [soullog] config — local logging only");
        None
    };

    // Start gateway — connects WS, sends register, starts read/heartbeat loops
    let (gw_event_tx, mut gw_event_rx) = mpsc::unbounded_channel();
    gateway.start(gw_event_tx).await.unwrap_or_else(|e| {
        tracing::error!(error = %e, "Failed to connect to shepherd");
        std::process::exit(1);
    });

    tracing::info!(
        url = %url,
        session_id = %session_id,
        identity_id = ?identity_id,
        max_turns,
        heartbeat_secs,
        "Shepherd gateway active — waiting for tasks"
    );

    // Load or create session state
    let mut session_state = state::SessionState::load(cwd, &session_id)
        .unwrap_or_else(|| state::SessionState::new(&session_id));

    // Message processing loop — each shepherd.task becomes an agent run
    loop {
        match gw_event_rx.recv().await {
            Some(GatewayEvent::MessageReceived {
                channel_id: _,
                sender: _,
                text,
                metadata,
            }) => {
                let is_steer = metadata
                    .get("steer")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if is_steer {
                    // Steering messages are injected mid-run. For now, log them.
                    // Full steering_tx integration is a future enhancement.
                    tracing::info!(text = %truncate(&text, 100), "Steer message received (not yet wired to steering_tx)");
                    continue;
                }

                // Override max_turns if shepherd specified it
                let task_max_turns = metadata
                    .get("max_turns")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(max_turns);

                // Extract profile from metadata (sent by shepherd with agent def)
                let profile_prompt = metadata
                    .get("profile")
                    .and_then(|p| p.get("systemPrompt"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                // Create per-agent working memory directory if profile has domains
                if let Some(profile) = metadata.get("profile") {
                    if let Some(domains) = profile.get("domains").and_then(|d| d.as_array()) {
                        if !domains.is_empty() {
                            let agent_dir = std::path::Path::new(cwd).join("agents").join("working-memory");
                            std::fs::create_dir_all(&agent_dir).ok();
                            tracing::info!(dir = %agent_dir.display(), "Per-agent working memory directory created");
                        }
                    }

                    // Execute session protocol onStart instructions
                    if let Some(on_start) = profile
                        .get("sessionProtocol")
                        .and_then(|sp| sp.get("onStart"))
                        .and_then(|os| os.as_array())
                    {
                        let instructions: Vec<&str> = on_start
                            .iter()
                            .filter_map(|v| v.as_str())
                            .collect();
                        if !instructions.is_empty() {
                            tracing::info!(
                                instructions = ?instructions,
                                "Session protocol onStart"
                            );
                        }
                    }
                }

                // Extract prior_messages from shepherd.task payload for resurrection
                let prior_messages: Vec<Message> = metadata
                    .get("prior_messages")
                    .and_then(|v| {
                        if v.is_null() {
                            None
                        } else {
                            serde_json::from_value::<Vec<Message>>(v.clone()).ok()
                        }
                    })
                    .unwrap_or_default();

                tracing::info!(
                    task = %truncate(&text, 100),
                    max_turns = task_max_turns,
                    has_profile = profile_prompt.is_some(),
                    prior_messages = prior_messages.len(),
                    "Received task from shepherd"
                );

                // Build initial_messages: shepherd prior history takes precedence over
                // local session_state (shepherd is the authoritative state store).
                // If shepherd sent prior_messages, use those; otherwise fall back to
                // local session_state (backwards compat with non-resurrection spawns).
                let mut initial_messages = if !prior_messages.is_empty() {
                    if prior_messages.len() != session_state.messages.len() {
                        tracing::info!(
                            prior = prior_messages.len(),
                            local = session_state.messages.len(),
                            "Resurrection: using shepherd prior_messages"
                        );
                    }
                    prior_messages
                } else {
                    session_state.messages.clone()
                };
                let user_msg = Message::user(&text);
                initial_messages.push(user_msg.clone());

                // Set up disk logging
                let logger = Arc::new(DiskLogger::new(&log_dir, &session_id));

                // Set up event forwarding — events go to WS + local disk + soullog
                let (agent_event_tx, agent_event_rx) = mpsc::unbounded_channel();
                let forwarder = shepherd_gateway::ShepherdEventForwarder::new(
                    gateway.sink(),
                    &session_id,
                );
                let forwarder_handle =
                    forwarder.spawn(agent_event_rx, Some(logger.clone()), soullog.clone());

                // Snapshot initial_messages before moving into agent (needed for result assembly)
                let initial_messages_snapshot = initial_messages.clone();

                // Run agent task (reuse the same function as other modes)
                let result = run_agent_task_with_event_tx(
                    provider.clone(),
                    config,
                    cwd,
                    initial_messages,
                    task_max_turns,
                    agent_event_tx,
                    profile_prompt.as_deref(),
                    agent_identity.as_ref(),
                )
                .await;

                // Wait for forwarder to flush remaining events
                forwarder_handle.await.ok();

                match result {
                    Ok(new_messages) => {
                        let response_text = new_messages
                            .last()
                            .map(|m| m.text_content())
                            .unwrap_or_default();

                        let turns_used = new_messages.len();

                        // Build full conversation (prior + new) for resurrection.
                        // initial_messages_snapshot already ends with user_msg.
                        let mut all_messages = initial_messages_snapshot;
                        all_messages.extend(new_messages.clone());

                        // Send result to shepherd with full message history for state persistence
                        if let Err(e) = gateway
                            .send_result_with_messages(&response_text, turns_used, &all_messages)
                            .await
                        {
                            tracing::error!(error = %e, "Failed to send result to shepherd");
                        }

                        // Persist session state locally too
                        session_state.messages = all_messages.clone();
                        session_state.total_turns += new_messages.len();
                        if let Err(e) = session_state.save(cwd) {
                            tracing::warn!(error = %e, "Failed to save session state");
                        }

                        logger.log_conversation(&log_dir, &all_messages);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Agent task failed");
                        let _ = gateway.send_result(&format!("Error: {e}"), 0).await;
                    }
                }
            }
            Some(GatewayEvent::Error { source, message }) => {
                tracing::error!(source = %source, error = %message, "Gateway error");
                if message.contains("Stop requested") || message.contains("closed") {
                    tracing::info!("Shepherd requested shutdown");
                    break;
                }
            }
            None => {
                tracing::info!("Gateway channel closed, shutting down");
                break;
            }
        }
    }

    gateway.stop().await.ok();
}

/// Agent task runner variant that accepts a pre-created event_tx channel.
/// Used by shepherd mode to wire events to the ShepherdEventForwarder.
async fn run_agent_task_with_event_tx(
    provider: Arc<soul_core::provider::balanced::BalancedProvider>,
    config: &config::AgentToml,
    cwd: &str,
    initial_messages: Vec<Message>,
    max_turns: usize,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    system_prompt_prefix: Option<&str>,
    agent_identity: Option<&identity::AgentIdentity>,
) -> Result<Vec<Message>, soul_core::error::SoulError> {
    let model = soul_core::types::ModelInfo {
        id: "balanced".into(),
        provider: soul_core::types::ProviderKind::Custom("balanced".into()),
        context_window: 128_000,
        max_output_tokens: 8192,
        supports_thinking: false,
        supports_tools: true,
        supports_images: false,
        cost_per_input_token: 0.0,
        cost_per_output_token: 0.0,
    };

    let context_strategy = match config.agent.context_strategy.as_str() {
        "rlm" => ContextStrategy::Rlm,
        "semantic_graph" => ContextStrategy::SemanticGraph,
        "classic" => ContextStrategy::Classic,
        _ => ContextStrategy::Rlm,
    };

    // Build system prompt: optional profile prefix + base SYSTEM_PROMPT
    let system_prompt = if let Some(prefix) = system_prompt_prefix {
        format!("{prefix}\n\n{}", prompt::SYSTEM_PROMPT)
    } else {
        prompt::SYSTEM_PROMPT.to_string()
    };

    let mut agent_config = AgentConfig::new(model.clone(), &system_prompt);
    agent_config.max_turns = Some(max_turns);
    agent_config.context_strategy = context_strategy;

    let (_steering_tx, steering_rx) = mpsc::unbounded_channel();

    let session_id = initial_messages
        .first()
        .map(|_| "shepherd")
        .unwrap_or("unknown")
        .to_string();

    let mut tool_registry = soul_coder::all_tools(
        Arc::new(NativeFs::new(cwd)),
        Arc::new(NativeExecutor),
        cwd,
    );

    tool_registry.register(Box::new(delegate::DelegateTool::new(
        provider.clone(),
        15,
        5,
        0,
        cwd.to_string(),
        event_tx.clone(),
        session_id.clone(),
    )));

    tool_registry.register(Box::new(plan_execute::PlanExecuteTool::new(
        provider.clone(),
        cwd.to_string(),
        event_tx.clone(),
        session_id.clone(),
    )));

    let skills_dir = std::path::Path::new(cwd).join(".amai-skills");
    let auth = soul_core::types::AuthProfile::new(
        soul_core::types::ProviderKind::Custom("balanced".into()),
        "",
    );
    tool_registry.register(Box::new(
        install_skill::InstallSkillTool::new(
            tool_registry.dynamic_handle(),
            provider.clone(),
            model.clone(),
            auth,
        )
        .with_persist_dir(skills_dir.clone()),
    ));

    if skills_dir.exists() {
        load_persisted_skills(&skills_dir, &tool_registry);
    }

    // Google tools: register if credentials are available
    if let Some(google_auth) = google::load_google_auth(None, None).await {
        google::register_google_tools(&mut tool_registry, google_auth);
        tracing::info!("Google tools enabled (7 tools)");
    }

    // AMAI contracts tool: register if identity is available
    if let Some(id) = agent_identity {
        if let Some(ref id_config) = config.identity {
            tool_registry.register(Box::new(ContractsTool::new(
                &id_config.id_service_url,
                &id.kid,
            )));
            tracing::info!(kid = %id.kid, "Contracts tool enabled");

            // Shepherd tool: spawn/manage sub-agent sessions
            let shepherd_api_url = config
                .shepherd
                .as_ref()
                .and_then(|s| s.api_url.clone())
                .unwrap_or_else(|| "http://localhost:8084".to_string());
            tool_registry.register(Box::new(ShepherdTool::new(
                &shepherd_api_url,
                &id_config.id_service_url,
                &id.kid,
                id.secret_key_bytes(),
            )));
            tracing::info!(url = %shepherd_api_url, "Shepherd tool enabled");
        }
    }

    // Web + network tools: web_search, fetch_url, http_request, arxiv, glob
    for tool in agent_tools_vec(cwd) {
        tool_registry.register(tool);
    }
    tracing::info!("Agent tools enabled (web_search, fetch_url, http_request, arxiv, glob)");

    let mut agent = AgentLoop::new(provider, tool_registry, agent_config);

    let options = RunOptions {
        session_id,
        initial_messages,
    };

    let result = agent.run(options, event_tx, steering_rx).await;
    drop(agent);
    result
}

// ─── Agent Task Runner ──────────────────────────────────────────────────────

async fn run_agent_task(
    provider: Arc<soul_core::provider::balanced::BalancedProvider>,
    config: &config::AgentToml,
    cwd: &str,
    initial_messages: Vec<Message>,
    max_turns: usize,
    logger: Arc<DiskLogger>,
    agent_identity: Option<&identity::AgentIdentity>,
    system_prompt_override: Option<&str>,
) -> Result<Vec<Message>, soul_core::error::SoulError> {
    let model = soul_core::types::ModelInfo {
        id: "balanced".into(),
        provider: soul_core::types::ProviderKind::Custom("balanced".into()),
        context_window: 128_000,
        max_output_tokens: 8192,
        supports_thinking: false,
        supports_tools: true,
        supports_images: false,
        cost_per_input_token: 0.0,
        cost_per_output_token: 0.0,
    };

    let context_strategy = match config.agent.context_strategy.as_str() {
        "rlm" => ContextStrategy::Rlm,
        "semantic_graph" => ContextStrategy::SemanticGraph,
        "classic" => ContextStrategy::Classic,
        _ => ContextStrategy::Rlm,
    };

    let effective_system_prompt = system_prompt_override.unwrap_or(prompt::SYSTEM_PROMPT);
    let mut agent_config = AgentConfig::new(model.clone(), effective_system_prompt);
    agent_config.max_turns = Some(max_turns);
    agent_config.context_strategy = context_strategy;

    // Create event channel first — DelegateTool needs event_tx for subagent events
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_steering_tx, steering_rx) = mpsc::unbounded_channel();

    let session_id = logger.session_id.clone();

    // Build tool registry: soul-coder (7) + delegate (1) = 8 tools.
    // All routing goes through ToolRegistry directly (no ExecutorRegistry).
    let mut tool_registry = soul_coder::all_tools(
        Arc::new(NativeFs::new(cwd)),
        Arc::new(NativeExecutor),
        cwd,
    );

    tool_registry.register(Box::new(delegate::DelegateTool::new(
        provider.clone(),
        15,  // default child turns
        5,   // max recursion depth
        0,   // current depth (root)
        cwd.to_string(),
        event_tx.clone(),
        session_id.clone(),
    )));

    tool_registry.register(Box::new(plan_execute::PlanExecuteTool::new(
        provider.clone(),
        cwd.to_string(),
        event_tx.clone(),
        session_id.clone(),
    )));

    // install_skill: LLM-powered skill installation from URL or raw content/docs
    let skills_dir = std::path::Path::new(cwd).join(".amai-skills");
    let auth = soul_core::types::AuthProfile::new(
        soul_core::types::ProviderKind::Custom("balanced".into()),
        "",
    );
    tool_registry.register(Box::new(
        install_skill::InstallSkillTool::new(
            tool_registry.dynamic_handle(),
            provider.clone(),
            model.clone(),
            auth,
        )
        .with_persist_dir(skills_dir.clone()),
    ));

    // Load persisted skills from .amai-skills/
    if skills_dir.exists() {
        load_persisted_skills(&skills_dir, &tool_registry);
    }

    // Google tools: register if credentials are available
    if let Some(google_auth) = google::load_google_auth(None, None).await {
        google::register_google_tools(&mut tool_registry, google_auth);
        tracing::info!("Google tools enabled (7 tools)");
    }

    // AMAI contracts tool: register if identity is available
    if let Some(id) = agent_identity {
        if let Some(ref id_config) = config.identity {
            tool_registry.register(Box::new(ContractsTool::new(
                &id_config.id_service_url,
                &id.kid,
            )));
            tracing::info!(kid = %id.kid, "Contracts tool enabled");

            // Shepherd tool: spawn/manage sub-agent sessions
            let shepherd_api_url = config
                .shepherd
                .as_ref()
                .and_then(|s| s.api_url.clone())
                .unwrap_or_else(|| "http://localhost:8084".to_string());
            tool_registry.register(Box::new(ShepherdTool::new(
                &shepherd_api_url,
                &id_config.id_service_url,
                &id.kid,
                id.secret_key_bytes(),
            )));
            tracing::info!(url = %shepherd_api_url, "Shepherd tool enabled");
        }
    }

    // Web + network tools: web_search, fetch_url, http_request, arxiv, glob
    for tool in agent_tools_vec(cwd) {
        tool_registry.register(tool);
    }
    tracing::info!("Agent tools enabled (web_search, fetch_url, http_request, arxiv, glob)");

    let mut agent = AgentLoop::new(provider, tool_registry, agent_config);

    let options = RunOptions {
        session_id: session_id.clone(),
        initial_messages,
    };

    // Event printer + logger — runs in background
    let logger_clone = logger.clone();
    let printer = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            // Log to disk
            logger_clone.log_event(&event);

            // Log to tracing
            match &event {
                AgentEvent::TurnStart { turn } => {
                    tracing::info!(session_id = %session_id, turn, "Turn started");
                }
                AgentEvent::MessageDelta { delta, .. } => {
                    if let soul_core::types::StreamDelta::TextDelta { text } = delta {
                        tracing::trace!(text_len = text.len(), "Text delta");
                    }
                }
                AgentEvent::ToolExecutionStart { tool_name, arguments, .. } => {
                    let args_preview = {
                        let s = arguments.to_string();
                        if s.len() > 200 { format!("{}...", &s[..200]) } else { s }
                    };
                    tracing::info!(
                        tool_name = %tool_name,
                        args = %args_preview,
                        "Tool executing"
                    );
                }
                AgentEvent::ToolExecutionEnd { result, .. } => {
                    if let soul_core::types::ContentBlock::ToolResult { content, is_error, .. } = result {
                        if *is_error {
                            tracing::warn!(result = %truncate(content, 500), "Tool error");
                        } else {
                            tracing::debug!(result = %truncate(content, 500), "Tool result");
                        }
                    }
                }
                AgentEvent::StructuralRetry { turn, attempt, failure_kind, .. } => {
                    tracing::warn!(turn, attempt, failure_kind = %failure_kind, "Structural retry");
                }
                AgentEvent::Error { message } => {
                    tracing::error!(message = %message, "Agent error");
                }
                AgentEvent::SubagentStart { child_session, purpose, max_turns, .. } => {
                    tracing::info!(
                        child = %child_session,
                        purpose = %purpose,
                        max_turns,
                        "Subagent spawned"
                    );
                }
                AgentEvent::SubagentEnd { child_session, purpose, turns_used, result_preview } => {
                    tracing::info!(
                        child = %child_session,
                        purpose = %purpose,
                        turns_used,
                        result = %truncate(&result_preview, 200),
                        "Subagent completed"
                    );
                }
                AgentEvent::PlanExecutionStart { task_count, .. } => {
                    tracing::info!(task_count, "Plan execution started");
                }
                AgentEvent::PlanTaskStart { task_id, description, .. } => {
                    tracing::info!(
                        task_id = %task_id,
                        description = %truncate(&description, 100),
                        "Plan task started"
                    );
                }
                AgentEvent::PlanTaskEnd { task_id, status, result_preview } => {
                    tracing::info!(
                        task_id = %task_id,
                        status = %status,
                        result = %truncate(&result_preview, 200),
                        "Plan task ended"
                    );
                }
                AgentEvent::PlanExecutionEnd { completed, failed, skipped, total } => {
                    tracing::info!(
                        completed, failed, skipped, total,
                        "Plan execution complete"
                    );
                }
                _ => {}
            }
        }
    });

    let result = agent.run(options, event_tx, steering_rx).await;
    // Drop the agent (which owns tool_registry → DelegateTool → event_tx clone)
    // before awaiting the printer. Otherwise: deadlock — printer waits for all
    // senders to drop, but agent holds a clone and won't drop until after printer.
    drop(agent);
    printer.await.ok();
    result
}

// ─── Single Task Mode ───────────────────────────────────────────────────────

async fn run_single_task(
    provider: Arc<soul_core::provider::balanced::BalancedProvider>,
    config: &config::AgentToml,
    cwd: &str,
    task: &str,
    max_turns: usize,
    context_files: &[PathBuf],
    timeout_mins: Option<u64>,
    resume: Option<&str>,
    purpose: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Load identity if configured
    let agent_identity = if let Some(ref id_config) = config.identity {
        let agent_name = id_config.name.clone().unwrap_or_else(|| {
            format!(
                "amai-agent-{}",
                hostname::get()
                    .map(|h| h.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "unknown".into())
            )
        });
        match identity::load_or_register(&id_config.id_service_url, &agent_name, &id_config.key_dir).await {
            Ok(id) => {
                tracing::info!(kid = %id.kid, identity_id = %id.identity_id, "Identity loaded");
                Some(id)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Identity registration failed — proceeding without identity");
                None
            }
        }
    } else {
        None
    };

    // Stage context files
    let mut context_note = String::new();
    if !context_files.is_empty() {
        let ctx_dir = std::path::Path::new(cwd).join(".amai-context");
        std::fs::create_dir_all(&ctx_dir).ok();

        for ctx_path in context_files {
            match std::fs::read_to_string(ctx_path) {
                Ok(content) => {
                    let filename = ctx_path
                        .file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_else(|| ctx_path.to_string_lossy().to_string());
                    let dest = ctx_dir.join(&filename);
                    if let Err(e) = std::fs::write(&dest, &content) {
                        tracing::warn!(
                            path = %dest.display(),
                            error = %e,
                            "Could not stage context file"
                        );
                        continue;
                    }
                    context_note.push_str(&format!(
                        "\nContext file staged: .amai-context/{filename} — read it on your first turn."
                    ));
                    tracing::info!(
                        source = %ctx_path.display(),
                        dest = %format!(".amai-context/{filename}"),
                        bytes = content.len(),
                        "Staged context file"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        path = %ctx_path.display(),
                        error = %e,
                        "Could not read context file"
                    );
                }
            }
        }
    }

    let full_task = if context_note.is_empty() {
        task.to_string()
    } else {
        format!("{task}\n{context_note}")
    };

    // Load or create session state
    let mut session_state = if let Some(resume_id) = resume {
        let loaded = if resume_id == "latest" {
            state::SessionState::load_latest(cwd)
        } else {
            state::SessionState::load(cwd, resume_id)
        };
        match loaded {
            Some(s) => {
                tracing::info!(
                    session_id = %s.session_id,
                    prior_messages = s.messages.len(),
                    prior_turns = s.total_turns,
                    "Resuming session"
                );
                s
            }
            None => {
                tracing::error!(resume = %resume_id, "Session not found");
                std::process::exit(1);
            }
        }
    } else {
        let session_id = format!(
            "amai-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );
        state::SessionState::new(&session_id)
    };

    let session_id = session_state.session_id.clone();
    let logger = Arc::new(DiskLogger::new(&config.logging.dir, &session_id));

    // Build initial_messages: prior history + new user message
    let mut initial_messages = session_state.messages.clone();
    let user_msg = Message::user(&full_task);
    initial_messages.push(user_msg.clone());

    let timeout_duration = timeout_mins.map(|mins| Duration::from_secs(mins * 60));

    // Select system prompt based on purpose
    let system_prompt_override: Option<String> = match purpose {
        Some("research") => {
            tracing::info!("Purpose: research — using RESEARCH_SYSTEM_PROMPT");
            Some(prompt::RESEARCH_SYSTEM_PROMPT.to_string())
        }
        _ => None,
    };
    let system_prompt_ref = system_prompt_override.as_deref();

    let result = if let Some(timeout) = timeout_duration {
        tracing::info!(timeout_mins = timeout.as_secs() / 60, "Timeout set");
        match tokio::time::timeout(
            timeout,
            run_agent_task(
                provider,
                config,
                cwd,
                initial_messages,
                max_turns,
                logger.clone(),
                agent_identity.as_ref(),
                system_prompt_ref,
            ),
        )
        .await
        {
            Ok(inner) => inner,
            Err(_) => {
                tracing::error!(timeout_mins = timeout.as_secs() / 60, "Agent exceeded timeout");
                std::process::exit(2);
            }
        }
    } else {
        run_agent_task(provider, config, cwd, initial_messages, max_turns, logger.clone(), agent_identity.as_ref(), system_prompt_ref).await
    };

    match result {
        Ok(new_messages) => {
            // Log final conversation (backward compat — all messages including prior)
            let mut all_messages = session_state.messages.clone();
            all_messages.push(user_msg);
            all_messages.extend(new_messages.clone());
            logger.log_conversation(&config.logging.dir, &all_messages);

            // Persist session state
            session_state.messages = all_messages;
            session_state.total_turns += new_messages.len();
            if let Err(e) = session_state.save(cwd) {
                tracing::warn!(error = %e, "Failed to save session state");
            }

            // Print final output
            if let Some(last) = new_messages.last() {
                let text = last.text_content();
                if !text.is_empty() {
                    println!("\n{text}");
                }
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

// ─── Supervisor Mode ────────────────────────────────────────────────────────

async fn run_supervisor() {
    tracing::info!("AMAI Supervisor starting");

    // Re-read config for supervisor settings
    let config = config::AgentToml::load_default().unwrap_or_else(|e| {
        tracing::error!(error = %e, "Config error");
        std::process::exit(1);
    });

    let max_restarts = config.supervisor.max_restarts;
    let restart_delay = config.supervisor.restart_delay_secs;
    let self_compile = config.supervisor.self_compile;
    let mut restart_count: usize = 0;

    loop {
        // Self-compile if enabled
        if self_compile {
            tracing::info!("Compiling amai-agent");
            let compile_status = tokio::process::Command::new("cargo")
                .args(["build", "--release"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .status()
                .await;

            match compile_status {
                Ok(status) if status.success() => {
                    tracing::info!("Compilation successful");
                }
                Ok(status) => {
                    tracing::warn!(status = %status, "Compilation failed, using existing binary");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Compilation error, using existing binary");
                }
            }
        }

        // Get current binary path and rebuild args (minus --supervisor)
        let current_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("amai"));
        let args: Vec<String> = std::env::args()
            .skip(1)
            .filter(|a| a != "--supervisor")
            .collect();

        tracing::info!(restart_count, "Starting agent");

        let child = tokio::process::Command::new(&current_exe)
            .args(&args)
            .status()
            .await;

        match child {
            Ok(status) => {
                tracing::info!(status = %status, "Agent exited");
            }
            Err(e) => {
                tracing::error!(error = %e, "Agent process error");
            }
        }

        restart_count += 1;
        if max_restarts > 0 && restart_count >= max_restarts {
            tracing::info!(max_restarts, "Max restarts reached, supervisor exiting");
            break;
        }

        tracing::info!(restart_delay, "Restarting");
        tokio::time::sleep(Duration::from_secs(restart_delay)).await;
    }
}

/// Load persisted skills from .amai-skills/ directory.
fn load_persisted_skills(skills_dir: &std::path::Path, registry: &ToolRegistry) {
    let entries = match std::fs::read_dir(skills_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(dir = %skills_dir.display(), error = %e, "Failed to read skills directory");
            return;
        }
    };

    let mut loaded = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "skill").unwrap_or(false) {
            if let Ok(content) = std::fs::read_to_string(&path) {
                match parse_skill(&content) {
                    Ok(skill) => {
                        let executor: Arc<dyn soul_core::skill::SkillExecutor> =
                            match &skill.execution {
                                SkillExecution::Lua { .. } => Arc::new(LuaSkillExecutor::new()),
                                SkillExecution::Shell { .. } => {
                                    Arc::new(ShellSkillExecutor::new(Arc::new(NativeExecutor)))
                                }
                                _ => continue,
                            };
                        let name = skill.name.clone();
                        let bridge = SkillToolBridge::new(skill, executor);
                        registry.dynamic_handle().register(Arc::new(bridge));
                        tracing::info!(
                            skill = %name,
                            path = %path.display(),
                            "Loaded persisted skill"
                        );
                        loaded += 1;
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "Failed to parse persisted skill"
                        );
                    }
                }
            }
        }
    }

    if loaded > 0 {
        tracing::info!(count = loaded, "Persisted skills loaded");
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

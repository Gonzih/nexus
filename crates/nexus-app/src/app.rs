use std::sync::Arc;

use soul_core::agent::{AgentLoop, RunOptions};
use soul_core::provider::{AnthropicProvider, OpenAIProvider};
use soul_core::tool::ToolRegistry;
use soul_core::vexec::NoopExecutor;
use soul_core::vfs::MemoryFs;
use soul_core::types::*;
use soul_terminal_app::SoulAppHandler;
use soul_terminal_core::*;
use soul_terminal_widgets::*;
use tokio::sync::mpsc;

use crate::config::{ProviderChoice, UserConfig};
use crate::onboarding::{OnboardingState, OnboardingStep};
use crate::ui::{self, ChatMessage, ChatRole};

/// Backend proxy URL — same-origin server handles proxy.
/// In browser WASM, relative URLs resolve against window.location.origin.
const PROXY_BASE_URL: &str = "";

/// Main Nexus application state.
pub struct AmaiApp {
    /// Current phase of the app
    phase: AppPhase,
    /// User config (API keys, model selection)
    config: UserConfig,
    /// Onboarding flow state
    onboarding: OnboardingState,
    /// Chat messages for display
    messages: Vec<ChatMessage>,
    /// User input widget
    input: InputWidget,
    /// In-memory virtual filesystem for agent workspace
    fs: Arc<MemoryFs>,
    /// Whether the agent is currently processing
    processing: bool,
    /// Token usage tracking
    total_tokens: usize,
    /// Total cost in USD
    total_cost: f64,
    /// Context usage percentage (0.0 - 1.0)
    context_pct: f32,
    /// Agent event receiver (populated when agent is running)
    event_rx: Option<mpsc::UnboundedReceiver<AgentEvent>>,
    /// Steering channel sender (to interrupt agent)
    steering_tx: Option<mpsc::UnboundedSender<Message>>,
}

#[derive(Debug, Clone, PartialEq)]
enum AppPhase {
    Onboarding,
    Chat,
}

impl AmaiApp {
    pub fn new() -> Self {
        let config = UserConfig::load();
        let phase = if config.has_api_key() {
            AppPhase::Chat
        } else {
            AppPhase::Onboarding
        };

        Self {
            phase,
            config,
            onboarding: OnboardingState::new(),
            messages: vec![ChatMessage {
                role: ChatRole::System,
                content: "Welcome to Nexus. How can I help you?".into(),
                tool_name: None,
            }],
            input: InputWidget::new("chat_input")
                .with_placeholder("Type a message..."),
            fs: Arc::new(MemoryFs::new()),
            processing: false,
            total_tokens: 0,
            total_cost: 0.0,
            context_pct: 0.0,
            event_rx: None,
            steering_tx: None,
        }
    }

    /// Build the agent's tool registry — combines soul-coder tools with Nexus tools.
    fn build_tools(&self) -> ToolRegistry {
        let executor = Arc::new(NoopExecutor);
        let cwd = "/workspace".to_string();

        // Start with soul-coder's all tools (bash is NoopExecutor in WASM)
        let registry = soul_coder::all_tools(
            self.fs.clone(),
            executor,
            cwd,
        );

        // Nexus-specific tools will be added when UiBridge is wired
        registry
    }

    /// Build the LLM provider based on user config.
    fn build_provider(&self) -> Arc<dyn soul_core::provider::Provider> {
        // Same-origin proxy: /api/anthropic/* and /api/openai/*
        match self.config.provider {
            ProviderChoice::Anthropic => {
                Arc::new(AnthropicProvider::with_base_url(
                    format!("{}/api/anthropic", PROXY_BASE_URL),
                ))
            }
            ProviderChoice::OpenAI => {
                Arc::new(OpenAIProvider::with_base_url(
                    format!("{}/api/openai", PROXY_BASE_URL),
                ))
            }
        }
    }

    /// Build ModelInfo for the selected model.
    fn model_info(&self) -> ModelInfo {
        match self.config.model.as_str() {
            "claude-opus-4-6" => ModelInfo {
                id: "claude-opus-4-6".into(),
                provider: ProviderKind::Anthropic,
                context_window: 200_000,
                max_output_tokens: 32_000,
                supports_thinking: true,
                supports_tools: true,
                supports_images: true,
                cost_per_input_token: 0.000015,
                cost_per_output_token: 0.000075,
            },
            "claude-sonnet-4-5-20250929" | _ => ModelInfo {
                id: "claude-sonnet-4-5-20250929".into(),
                provider: ProviderKind::Anthropic,
                context_window: 200_000,
                max_output_tokens: 16_000,
                supports_thinking: true,
                supports_tools: true,
                supports_images: true,
                cost_per_input_token: 0.000003,
                cost_per_output_token: 0.000015,
            },
        }
    }

    /// Send a user message and kick off the agent loop.
    pub fn send_message(&mut self, text: String) {
        if text.trim().is_empty() || self.processing {
            return;
        }

        self.messages.push(ChatMessage {
            role: ChatRole::User,
            content: text.clone(),
            tool_name: None,
        });

        self.processing = true;

        // Spawn agent loop
        let provider = self.build_provider();
        let tools = self.build_tools();
        let model = self.model_info();
        let system_prompt = self
            .config
            .system_prompt
            .clone()
            .unwrap_or_else(|| {
                "You are Nexus, a helpful AI agent running in the browser. \
                 You have access to coding tools (read, write, edit, grep, find, ls) \
                 that operate on an in-memory filesystem. \
                 Be concise and helpful."
                    .into()
            });

        let config = AgentConfig {
            model,
            system_prompt,
            max_turns: Some(20),
            compaction_threshold: 0.75,
            token_safety_margin: 1.2,
            fallback_models: vec![],
            context_strategy: ContextStrategy::default(),
        };

        let _auth = AuthProfile::new(
            match self.config.provider {
                ProviderChoice::Anthropic => ProviderKind::Anthropic,
                ProviderChoice::OpenAI => ProviderKind::OpenAI,
            },
            self.config.active_key().unwrap_or("").to_string(),
        );

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (steering_tx, steering_rx) = mpsc::unbounded_channel();

        self.event_rx = Some(event_rx);
        self.steering_tx = Some(steering_tx);

        let initial_messages = vec![Message::user(&text)];

        // Spawn the agent loop via wasm_bindgen_futures
        wasm_bindgen_futures::spawn_local(async move {
            let mut agent = AgentLoop::new(provider, tools, config);
            let options = RunOptions {
                session_id: "nexus-session".into(),
                initial_messages,
            };
            let _ = agent.run(options, event_tx, steering_rx).await;
        });
    }

    /// Poll for agent events and update UI state.
    /// Called every frame from the render loop.
    pub fn poll_events(&mut self) {
        let mut should_clear = false;

        if let Some(rx) = &mut self.event_rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    AgentEvent::MessageDelta {
                        delta: StreamDelta::TextDelta { text },
                        ..
                    } => {
                        // Append to last assistant message or create new one
                        let append = self
                            .messages
                            .last()
                            .map(|m| m.role == ChatRole::Assistant)
                            .unwrap_or(false);
                        if append {
                            self.messages.last_mut().unwrap().content.push_str(&text);
                        } else {
                            self.messages.push(ChatMessage {
                                role: ChatRole::Assistant,
                                content: text,
                                tool_name: None,
                            });
                        }
                    }
                    AgentEvent::ToolExecutionStart {
                        tool_name,
                        tool_call_id: _,
                        arguments: _,
                    } => {
                        self.messages.push(ChatMessage {
                            role: ChatRole::Tool,
                            content: format!("Running {tool_name}..."),
                            tool_name: Some(tool_name),
                        });
                    }
                    AgentEvent::ToolExecutionEnd {
                        tool_call_id: _,
                        result,
                    } => {
                        // Extract text from ContentBlock
                        let result_text = match &result {
                            ContentBlock::ToolResult { content, .. } => content.clone(),
                            ContentBlock::Text { text } => text.clone(),
                            _ => String::new(),
                        };
                        if let Some(last) = self.messages.last_mut() {
                            if last.role == ChatRole::Tool {
                                let preview = if result_text.len() > 200 {
                                    format!("{}...", &result_text[..200])
                                } else {
                                    result_text
                                };
                                let tool = last.tool_name.clone().unwrap_or_default();
                                last.content = format!("[{tool}] {preview}");
                            }
                        }
                    }
                    AgentEvent::TurnEnd { message, .. } => {
                        if let Some(usage) = &message.usage {
                            self.total_tokens += usage.input_tokens + usage.output_tokens;
                        }
                    }
                    AgentEvent::AgentEnd { .. } => {
                        self.processing = false;
                        should_clear = true;
                    }
                    AgentEvent::Cost(cost_event) => {
                        self.total_cost = cost_event.cumulative_cost_usd;
                    }
                    AgentEvent::Error { message, .. } => {
                        self.messages.push(ChatMessage {
                            role: ChatRole::System,
                            content: format!("Error: {message}"),
                            tool_name: None,
                        });
                        self.processing = false;
                        should_clear = true;
                    }
                    _ => {}
                }
            }
        }

        if should_clear {
            self.event_rx = None;
            self.steering_tx = None;
        }
    }
}

impl SoulAppHandler for AmaiApp {
    fn setup(&mut self, _theme: &Theme) {
        log::info!("Nexus initialized");
    }

    fn update(&mut self) -> Vec<Box<dyn Widget>> {
        // Poll for agent events
        self.poll_events();

        match &self.phase {
            AppPhase::Onboarding => self.onboarding.render(),
            AppPhase::Chat => {
                let mut root = Container::new("root")
                    .with_block_style(BlockStyle::default().with_padding(Spacing::all(0.0)))
                    .with_gap(0.0);

                // Header
                let header = Container::new("header")
                    .with_block_style(
                        BlockStyle::default()
                            .with_padding(Spacing::symmetric(8.0, 16.0))
                            .with_background(Color::from_hex("#1a1b26")),
                    )
                    .with_direction(Direction::Row)
                    .with_gap(12.0)
                    .add_child(Box::new(
                        TextWidget::new("logo", "Nexus")
                            .with_font_size(16.0)
                            .with_bold(true)
                            .with_color(Color::from_hex("#7aa2f7")),
                    ))
                    .add_child(Box::new(
                        TextWidget::new("by", "by Nexus")
                            .with_font_size(12.0)
                            .with_color(Color::from_hex("#565f89")),
                    ));

                root = root.add_child(Box::new(header));

                // Messages area
                let messages_area = ui::render_messages(&self.messages);
                root = root.add_child(Box::new(ScrollView::new("scroll", Box::new(messages_area))));

                // Processing indicator
                if self.processing {
                    root = root.add_child(Box::new(
                        TextWidget::new("processing", "Nexus is thinking...")
                            .with_color(Color::from_hex("#e0af68"))
                            .with_font_size(12.0),
                    ));
                }

                // Status bar
                let status = ui::render_status_bar(
                    &self.config.model,
                    self.total_tokens,
                    self.total_cost,
                    self.context_pct,
                );
                root = root.add_child(Box::new(status));

                vec![Box::new(root)]
            }
        }
    }

    fn on_event(&mut self, event: &Event) -> EventResponse {
        match &self.phase {
            AppPhase::Onboarding => {
                match event {
                    Event::KeyDown {
                        key: KeyEvent::Named(Key::Enter),
                        ..
                    } => {
                        match self.onboarding.step {
                            OnboardingStep::Welcome => {
                                self.onboarding.advance();
                                return EventResponse::Consumed;
                            }
                            OnboardingStep::EnterKey => {
                                if let Some(config) = self.onboarding.validate_key() {
                                    self.config = config;
                                    self.phase = AppPhase::Chat;
                                }
                                return EventResponse::Consumed;
                            }
                            _ => {}
                        }
                    }
                    Event::KeyDown {
                        key: KeyEvent::Char('1'),
                        ..
                    } if self.onboarding.step == OnboardingStep::ChooseProvider => {
                        self.onboarding.select_provider(ProviderChoice::Anthropic);
                        return EventResponse::Consumed;
                    }
                    Event::KeyDown {
                        key: KeyEvent::Char('2'),
                        ..
                    } if self.onboarding.step == OnboardingStep::ChooseProvider => {
                        self.onboarding.select_provider(ProviderChoice::OpenAI);
                        return EventResponse::Consumed;
                    }
                    _ => {}
                }
                self.onboarding.on_event(event)
            }
            AppPhase::Chat => {
                match event {
                    Event::KeyDown {
                        key: KeyEvent::Named(Key::Enter),
                        ..
                    } => {
                        let text = self.input.value().to_string();
                        if !text.is_empty() {
                            self.send_message(text);
                            self.input.set_value("");
                        }
                        EventResponse::Consumed
                    }
                    _ => {
                        let rect = Rect::new(0.0, 0.0, 800.0, 40.0);
                        self.input.on_event(event, rect)
                    }
                }
            }
        }
    }
}

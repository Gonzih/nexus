use soul_terminal_core::*;
use soul_terminal_widgets::*;

use crate::config::{ProviderChoice, UserConfig};

/// Onboarding flow state machine.
#[derive(Debug, Clone, PartialEq)]
pub enum OnboardingStep {
    Welcome,
    ChooseProvider,
    EnterKey,
    Validating,
    Connected,
}

pub struct OnboardingState {
    pub step: OnboardingStep,
    pub provider: ProviderChoice,
    pub key_input: InputWidget,
    pub error_message: Option<String>,
}

impl OnboardingState {
    pub fn new() -> Self {
        Self {
            step: OnboardingStep::Welcome,
            provider: ProviderChoice::Anthropic,
            key_input: InputWidget::new("api_key_input")
                .with_placeholder("Paste your API key here..."),
            error_message: None,
        }
    }

    /// Advance to next step based on current state.
    pub fn advance(&mut self) {
        self.step = match self.step {
            OnboardingStep::Welcome => OnboardingStep::ChooseProvider,
            OnboardingStep::ChooseProvider => OnboardingStep::EnterKey,
            OnboardingStep::EnterKey => OnboardingStep::Validating,
            OnboardingStep::Validating => OnboardingStep::Connected,
            OnboardingStep::Connected => OnboardingStep::Connected,
        };
    }

    /// Select provider and advance.
    pub fn select_provider(&mut self, provider: ProviderChoice) {
        self.provider = provider;
        self.step = OnboardingStep::EnterKey;
    }

    /// Validate key format and return config if valid.
    pub fn validate_key(&mut self) -> Option<UserConfig> {
        let key = self.key_input.value().trim().to_string();

        let valid = match self.provider {
            ProviderChoice::Anthropic => key.starts_with("sk-ant-"),
            ProviderChoice::OpenAI => key.starts_with("sk-"),
        };

        if !valid {
            self.error_message = Some(match self.provider {
                ProviderChoice::Anthropic => "Key must start with sk-ant-".into(),
                ProviderChoice::OpenAI => "Key must start with sk-".into(),
            });
            return None;
        }

        self.error_message = None;
        self.step = OnboardingStep::Connected;

        let mut config = UserConfig::default();
        config.provider = self.provider.clone();
        match self.provider {
            ProviderChoice::Anthropic => config.anthropic_key = Some(key),
            ProviderChoice::OpenAI => config.openai_key = Some(key),
        }
        config.save();
        Some(config)
    }

    /// Handle keyboard events during onboarding.
    pub fn on_event(&mut self, event: &Event) -> EventResponse {
        match &self.step {
            OnboardingStep::EnterKey => {
                // Forward to input widget
                let rect = Rect::new(0.0, 0.0, 600.0, 40.0);
                self.key_input.on_event(event, rect)
            }
            _ => EventResponse::Ignored,
        }
    }

    /// Render current onboarding step as widgets.
    pub fn render(&self) -> Vec<Box<dyn Widget>> {
        match &self.step {
            OnboardingStep::Welcome => self.render_welcome(),
            OnboardingStep::ChooseProvider => self.render_choose_provider(),
            OnboardingStep::EnterKey => self.render_enter_key(),
            OnboardingStep::Validating => self.render_validating(),
            OnboardingStep::Connected => self.render_connected(),
        }
    }

    fn render_welcome(&self) -> Vec<Box<dyn Widget>> {
        let root = Container::new("onboarding")
            .with_block_style(BlockStyle::default().with_padding(Spacing::all(32.0)))
            .with_gap(16.0)
            .add_child(Box::new(
                TextWidget::new("logo", "Nexus")
                    .with_font_size(48.0)
                    .with_bold(true)
                    .with_color(Color::from_hex("#7aa2f7")),
            ))
            .add_child(Box::new(
                TextWidget::new("subtitle", "by Nexus")
                    .with_font_size(16.0)
                    .with_color(Color::from_hex("#565f89")),
            ))
            .add_child(Box::new(Separator::new("sep1")))
            .add_child(Box::new(TextWidget::new(
                "desc",
                "A browser-first general-purpose AI agent.",
            )))
            .add_child(Box::new(TextWidget::new(
                "desc2",
                "Powered by soul-core. Rendered by soul-terminal.",
            )))
            .add_child(Box::new(Separator::new("sep2")))
            .add_child(Box::new(TextWidget::new(
                "cta",
                "Press Enter to get started...",
            ).with_color(Color::from_hex("#9ece6a"))));

        vec![Box::new(root)]
    }

    fn render_choose_provider(&self) -> Vec<Box<dyn Widget>> {
        let root = Container::new("onboarding")
            .with_block_style(BlockStyle::default().with_padding(Spacing::all(32.0)))
            .with_gap(12.0)
            .add_child(Box::new(
                TextWidget::new("title", "Choose your LLM provider")
                    .with_font_size(24.0)
                    .with_bold(true),
            ))
            .add_child(Box::new(Separator::new("sep")))
            .add_child(Box::new(TextWidget::new(
                "opt1",
                "[1] Anthropic (Claude) — recommended",
            ).with_color(Color::from_hex("#7aa2f7"))))
            .add_child(Box::new(TextWidget::new(
                "opt2",
                "[2] OpenAI (GPT)",
            ).with_color(Color::from_hex("#9ece6a"))))
            .add_child(Box::new(Separator::new("sep2")))
            .add_child(Box::new(TextWidget::new(
                "hint",
                "Press 1 or 2 to select...",
            ).with_color(Color::from_hex("#565f89"))));

        vec![Box::new(root)]
    }

    fn render_enter_key(&self) -> Vec<Box<dyn Widget>> {
        let provider_name = match self.provider {
            ProviderChoice::Anthropic => "Anthropic",
            ProviderChoice::OpenAI => "OpenAI",
        };

        let key_url = match self.provider {
            ProviderChoice::Anthropic => "https://console.anthropic.com/settings/keys",
            ProviderChoice::OpenAI => "https://platform.openai.com/api-keys",
        };

        let mut root = Container::new("onboarding")
            .with_block_style(BlockStyle::default().with_padding(Spacing::all(32.0)))
            .with_gap(12.0)
            .add_child(Box::new(
                TextWidget::new("title", format!("Enter your {provider_name} API key"))
                    .with_font_size(24.0)
                    .with_bold(true),
            ))
            .add_child(Box::new(Separator::new("sep")))
            .add_child(Box::new(
                TextWidget::new("url", format!("Get your key at: {key_url}"))
                    .with_color(Color::from_hex("#7aa2f7")),
            ))
            .add_child(Box::new(TextWidget::new(
                "security",
                "Your key stays in your browser (localStorage). It is only sent to the LLM provider.",
            ).with_color(Color::from_hex("#565f89"))));

        if let Some(err) = &self.error_message {
            root = root.add_child(Box::new(
                TextWidget::new("error", err.clone())
                    .with_color(Color::from_hex("#f7768e")),
            ));
        }

        root = root
            .add_child(Box::new(Separator::new("sep2")))
            .add_child(Box::new(TextWidget::new(
                "hint",
                "Paste your key and press Enter...",
            ).with_color(Color::from_hex("#9ece6a"))));

        vec![Box::new(root)]
    }

    fn render_validating(&self) -> Vec<Box<dyn Widget>> {
        let root = Container::new("onboarding")
            .with_block_style(BlockStyle::default().with_padding(Spacing::all(32.0)))
            .with_gap(12.0)
            .add_child(Box::new(TextWidget::new(
                "msg",
                "Validating API key...",
            )));
        vec![Box::new(root)]
    }

    fn render_connected(&self) -> Vec<Box<dyn Widget>> {
        let root = Container::new("onboarding")
            .with_block_style(BlockStyle::default().with_padding(Spacing::all(32.0)))
            .with_gap(12.0)
            .add_child(Box::new(
                TextWidget::new("check", "Connected!")
                    .with_font_size(24.0)
                    .with_bold(true)
                    .with_color(Color::from_hex("#9ece6a")),
            ))
            .add_child(Box::new(TextWidget::new(
                "msg",
                "Starting Nexus agent...",
            )));
        vec![Box::new(root)]
    }
}

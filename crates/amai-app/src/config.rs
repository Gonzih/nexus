use serde::{Deserialize, Serialize};

/// User configuration persisted in browser localStorage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserConfig {
    /// Anthropic API key (sk-ant-api03-* or sk-ant-oat01-*)
    pub anthropic_key: Option<String>,
    /// OpenAI API key (sk-*)
    pub openai_key: Option<String>,
    /// Currently selected provider
    pub provider: ProviderChoice,
    /// Currently selected model ID
    pub model: String,
    /// Custom system prompt (optional override)
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderChoice {
    Anthropic,
    OpenAI,
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            anthropic_key: None,
            openai_key: None,
            provider: ProviderChoice::Anthropic,
            model: "claude-sonnet-4-5-20250929".into(),
            system_prompt: None,
        }
    }
}

impl UserConfig {
    #[allow(dead_code)]
    const STORAGE_KEY: &'static str = "amai_config";

    /// Load config from browser localStorage.
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        {
            if let Some(window) = web_sys::window() {
                if let Ok(Some(storage)) = window.local_storage() {
                    if let Ok(Some(json)) = storage.get_item(Self::STORAGE_KEY) {
                        if let Ok(config) = serde_json::from_str::<UserConfig>(&json) {
                            return config;
                        }
                    }
                }
            }
        }
        Self::default()
    }

    /// Save config to browser localStorage.
    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        {
            if let Some(window) = web_sys::window() {
                if let Ok(Some(storage)) = window.local_storage() {
                    if let Ok(json) = serde_json::to_string(self) {
                        let _ = storage.set_item(Self::STORAGE_KEY, &json);
                    }
                }
            }
        }
    }

    /// Check if user has provided a valid API key for the selected provider.
    pub fn has_api_key(&self) -> bool {
        match self.provider {
            ProviderChoice::Anthropic => self
                .anthropic_key
                .as_ref()
                .map(|k| k.starts_with("sk-ant-"))
                .unwrap_or(false),
            ProviderChoice::OpenAI => self
                .openai_key
                .as_ref()
                .map(|k| k.starts_with("sk-"))
                .unwrap_or(false),
        }
    }

    /// Get the active API key.
    pub fn active_key(&self) -> Option<&str> {
        match self.provider {
            ProviderChoice::Anthropic => self.anthropic_key.as_deref(),
            ProviderChoice::OpenAI => self.openai_key.as_deref(),
        }
    }
}

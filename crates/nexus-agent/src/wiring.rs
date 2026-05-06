use std::sync::Arc;

use soul_core::provider::balanced::intent::Intent;
use soul_core::provider::balanced::rate_limit::RateLimitTracker;
use soul_core::provider::balanced::{BalancedProvider, IntentMapping, Strategy};
use soul_core::provider::{OllamaProvider, OpenAIProvider, Provider};
use soul_core::types::{AuthProfile, ModelInfo, ProviderKind};

use crate::config::{AgentToml, ProviderConfig};

/// Build a BalancedProvider from TOML config
pub fn build_balanced(config: &AgentToml) -> Result<BalancedProvider, String> {
    let strategy = match config.agent.strategy.as_str() {
        "round_robin" => Strategy::RoundRobin,
        "weighted" => Strategy::Weighted,
        "least_loaded" => Strategy::LeastLoaded,
        "failover" => Strategy::Failover,
        other => return Err(format!("Unknown strategy: {other}")),
    };

    let mut balanced = BalancedProvider::new(strategy);

    for (name, pc) in &config.providers {
        let (provider, auth) = build_provider_slot(name, pc)?;
        let model = build_model_info(name, pc);
        let rate_limit = RateLimitTracker::new(pc.rpm, pc.rpd, pc.tpm);

        balanced.add_slot(name.clone(), provider, model, auth, pc.weight, rate_limit);
    }

    // Wire intent mappings
    for (intent_name, ic) in &config.intents {
        let intent = Intent::from_str(intent_name);
        let mapping = IntentMapping {
            preferred: ic.preferred.clone(),
            models: ic.models.clone(),
        };
        balanced.map_intent(intent, mapping);
    }

    Ok(balanced)
}

fn build_provider_slot(
    name: &str,
    pc: &ProviderConfig,
) -> Result<(Arc<dyn Provider>, AuthProfile), String> {
    let api_key = if let Some(ref env_var) = pc.api_key_env {
        std::env::var(env_var).unwrap_or_default()
    } else {
        String::new()
    };

    match pc.kind.as_str() {
        "openai" => {
            let mut provider = OpenAIProvider::new();
            if let Some(ref url) = pc.base_url {
                provider = OpenAIProvider::with_base_url(url);
            }
            let mut auth = AuthProfile::new(ProviderKind::OpenAI, &api_key);
            if let Some(ref url) = pc.base_url {
                auth.base_url = Some(url.clone());
            }
            Ok((Arc::new(provider), auth))
        }
        "anthropic" => {
            let provider = soul_core::provider::AnthropicProvider::new();
            let auth = AuthProfile::new(ProviderKind::Anthropic, &api_key);
            Ok((Arc::new(provider), auth))
        }
        "ollama" => {
            let provider = if let Some(ref url) = pc.base_url {
                OllamaProvider::with_base_url(url)
            } else {
                OllamaProvider::new()
            };
            let auth = AuthProfile::new(ProviderKind::Ollama, "");
            Ok((Arc::new(provider), auth))
        }
        other => Err(format!("Unknown provider kind '{other}' for slot '{name}'")),
    }
}

fn build_model_info(_name: &str, pc: &ProviderConfig) -> ModelInfo {
    let provider_kind = match pc.kind.as_str() {
        "anthropic" => ProviderKind::Anthropic,
        "ollama" => ProviderKind::Ollama,
        _ => ProviderKind::OpenAI,
    };

    ModelInfo {
        id: pc.model.clone(),
        provider: provider_kind,
        context_window: pc.context_window,
        max_output_tokens: pc.max_output_tokens,
        supports_thinking: false,
        supports_tools: true,
        supports_images: false,
        cost_per_input_token: 0.0,
        cost_per_output_token: 0.0,
    }
}

/// Build a default single-provider setup when no config is found.
/// Uses Ollama as the default local provider.
pub fn build_default_ollama() -> BalancedProvider {
    let mut balanced = BalancedProvider::new(Strategy::Failover);

    let provider = Arc::new(OllamaProvider::new());
    let model = ModelInfo {
        id: "qwen2.5-coder:7b".into(),
        provider: ProviderKind::Ollama,
        context_window: 32_768,
        max_output_tokens: 8192,
        supports_thinking: false,
        supports_tools: true,
        supports_images: false,
        cost_per_input_token: 0.0,
        cost_per_output_token: 0.0,
    };
    let auth = AuthProfile::new(ProviderKind::Ollama, "");

    balanced.add_slot(
        "ollama",
        provider,
        model,
        auth,
        10,
        RateLimitTracker::unlimited(),
    );

    balanced
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn test_config(
        agent: crate::config::AgentSection,
        providers: HashMap<String, ProviderConfig>,
        intents: HashMap<String, crate::config::IntentConfig>,
    ) -> AgentToml {
        AgentToml {
            agent,
            providers,
            intents,
            telegram: None,
            shepherd: None,
            identity: None,
            soullog: None,
            logging: Default::default(),
            supervisor: Default::default(),
        }
    }

    #[test]
    fn build_from_empty_config() {
        let config = test_config(Default::default(), HashMap::new(), HashMap::new());
        let balanced = build_balanced(&config).unwrap();
        let status = balanced.status();
        assert_eq!(status.total_slots, 0);
    }

    #[test]
    fn build_default_ollama_has_one_slot() {
        let balanced = build_default_ollama();
        let status = balanced.status();
        assert_eq!(status.total_slots, 1);
        assert_eq!(status.slots[0].name, "ollama");
    }

    #[test]
    fn unknown_strategy_errors() {
        let config = test_config(
            crate::config::AgentSection {
                strategy: "chaos".into(),
                max_turns: 10,
                cwd: ".".into(),
                context_strategy: "rlm".into(),
            },
            HashMap::new(),
            HashMap::new(),
        );
        assert!(build_balanced(&config).is_err());
    }

    #[test]
    fn unknown_provider_kind_errors() {
        let mut providers = HashMap::new();
        providers.insert(
            "bad".into(),
            ProviderConfig {
                kind: "nonexistent".into(),
                api_key_env: None,
                base_url: None,
                model: "m".into(),
                context_window: 128_000,
                max_output_tokens: 8192,
                weight: 10,
                rpm: None,
                rpd: None,
                tpm: None,
            },
        );
        let config = test_config(Default::default(), providers, HashMap::new());
        assert!(build_balanced(&config).is_err());
    }
}

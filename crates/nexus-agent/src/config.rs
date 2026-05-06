use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

/// Top-level agent configuration (TOML)
#[derive(Debug, Deserialize)]
pub struct AgentToml {
    #[serde(default)]
    pub agent: AgentSection,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub intents: HashMap<String, IntentConfig>,
    #[serde(default)]
    pub telegram: Option<TelegramConfig>,
    #[serde(default)]
    pub shepherd: Option<ShepherdConfig>,
    #[serde(default)]
    pub identity: Option<IdentityConfig>,
    #[serde(default)]
    pub soullog: Option<SoullogConfig>,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub supervisor: SupervisorConfig,
}

#[derive(Debug, Deserialize)]
pub struct AgentSection {
    #[serde(default = "default_strategy")]
    pub strategy: String,
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    /// Context management strategy: "classic", "rlm", or "semantic_graph"
    #[serde(default = "default_context_strategy")]
    pub context_strategy: String,
}

impl Default for AgentSection {
    fn default() -> Self {
        Self {
            strategy: default_strategy(),
            max_turns: default_max_turns(),
            cwd: default_cwd(),
            context_strategy: default_context_strategy(),
        }
    }
}

fn default_strategy() -> String {
    "failover".into()
}
fn default_max_turns() -> usize {
    50
}
fn default_cwd() -> String {
    ".".into()
}
fn default_context_strategy() -> String {
    "rlm".into()
}

#[derive(Debug, Deserialize)]
pub struct ProviderConfig {
    pub kind: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    pub model: String,
    #[serde(default = "default_context_window")]
    pub context_window: usize,
    #[serde(default = "default_max_output")]
    pub max_output_tokens: usize,
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(default)]
    pub rpm: Option<u32>,
    #[serde(default)]
    pub rpd: Option<u32>,
    #[serde(default)]
    pub tpm: Option<u64>,
}

fn default_context_window() -> usize {
    128_000
}
fn default_max_output() -> usize {
    8192
}
fn default_weight() -> u32 {
    10
}

#[derive(Debug, Deserialize)]
pub struct IntentConfig {
    pub preferred: Vec<String>,
    #[serde(default)]
    pub models: HashMap<String, String>,
}

/// Telegram gateway configuration
#[derive(Debug, Deserialize)]
pub struct TelegramConfig {
    /// Bot token from @BotFather
    pub token_env: Option<String>,
    /// Direct token (use token_env in production)
    pub token: Option<String>,
    /// Whitelisted user IDs or @usernames
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Chat ID to send startup notification to
    pub startup_chat_id: Option<String>,
    /// Optional system prompt prefix injected before the base system prompt.
    /// Use this to set the agent's identity, persona, or gateway-specific instructions.
    pub system_prompt: Option<String>,
    /// Max number of prior messages to load from session history per request.
    /// Prevents context overflow on small-window local models. Default: unlimited.
    pub history_window: Option<usize>,
}

impl TelegramConfig {
    pub fn resolve_token(&self) -> Option<String> {
        if let Some(ref env_var) = self.token_env {
            let val = std::env::var(env_var).ok();
            if val.as_ref().map(|v| !v.is_empty()).unwrap_or(false) {
                return val;
            }
        }
        self.token.clone()
    }
}

/// Shepherd gateway configuration (managed execution via WebSocket)
#[derive(Debug, Deserialize)]
pub struct ShepherdConfig {
    /// WebSocket URL (e.g. ws://shepherd:8084/ws/sessions/{session_id})
    pub url: Option<String>,
    /// HTTP API URL for the shepherd tool to spawn/manage sessions (default: http://localhost:8084)
    pub api_url: Option<String>,
    /// Heartbeat interval in seconds (default: 15)
    pub heartbeat_secs: Option<u64>,
}

/// Identity configuration — Ed25519 keypair + id-service registration
#[derive(Debug, Deserialize)]
pub struct IdentityConfig {
    /// URL of the id-service (default: http://localhost:8080)
    #[serde(default = "default_id_service_url")]
    pub id_service_url: String,
    /// Agent name for registration (default: hostname-based)
    pub name: Option<String>,
    /// Directory for identity keys (default: ~/.nexus/identity/)
    pub key_dir: Option<String>,
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            id_service_url: default_id_service_url(),
            name: None,
            key_dir: None,
        }
    }
}

fn default_id_service_url() -> String {
    "http://localhost:8080".into()
}

/// Soullog configuration — centralized logging to soul-log-service
#[derive(Debug, Deserialize)]
pub struct SoullogConfig {
    /// URL of the soul-log-service (default: http://localhost:8086)
    #[serde(default = "default_soullog_url")]
    pub url: String,
    /// Service name in soullog events (default: nexus-agent)
    #[serde(default = "default_soullog_service")]
    pub service: String,
    /// Max events per batch before flush (default: 100)
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    /// Flush interval in milliseconds (default: 1000)
    #[serde(default = "default_flush_interval")]
    pub flush_interval_ms: u64,
}

impl Default for SoullogConfig {
    fn default() -> Self {
        Self {
            url: default_soullog_url(),
            service: default_soullog_service(),
            batch_size: default_batch_size(),
            flush_interval_ms: default_flush_interval(),
        }
    }
}

fn default_soullog_url() -> String {
    "http://localhost:8086".into()
}
fn default_soullog_service() -> String {
    "nexus-agent".into()
}
fn default_batch_size() -> usize {
    100
}
fn default_flush_interval() -> u64 {
    1000
}

/// Logging configuration for disk persistence
#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    /// Directory for session logs (default: .nexus-logs/)
    #[serde(default = "default_log_dir")]
    pub dir: String,
    /// Log every agent event as JSONL
    #[serde(default = "default_true")]
    #[allow(dead_code)]
    pub events: bool,
    /// Log full conversation history on each turn
    #[serde(default)]
    #[allow(dead_code)]
    pub conversations: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            dir: default_log_dir(),
            events: true,
            conversations: false,
        }
    }
}

fn default_log_dir() -> String {
    ".nexus-logs".into()
}

fn default_true() -> bool {
    true
}

/// Supervisor configuration for auto-restart
#[derive(Debug, Deserialize)]
pub struct SupervisorConfig {
    /// Enable supervisor mode (auto-restart on exit)
    #[serde(default)]
    #[allow(dead_code)]
    pub enabled: bool,
    /// Self-compile before restart
    #[serde(default)]
    pub self_compile: bool,
    /// Max restarts before giving up (0 = unlimited)
    #[serde(default)]
    pub max_restarts: usize,
    /// Delay between restarts in seconds
    #[serde(default = "default_restart_delay")]
    pub restart_delay_secs: u64,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            self_compile: false,
            max_restarts: 0,
            restart_delay_secs: default_restart_delay(),
        }
    }
}

fn default_restart_delay() -> u64 {
    3
}

impl AgentToml {
    pub fn load(path: &Path) -> Result<Self, String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("Failed to read config: {e}"))?;
        toml::from_str(&content).map_err(|e| format!("Failed to parse config: {e}"))
    }

    /// Load from default locations: ./nexus-agent.toml, ~/.config/nexus/agent.toml
    pub fn load_default() -> Result<Self, String> {
        let candidates = [
            "nexus-agent.toml".to_string(),
            dirs_candidate(),
        ];

        for path in &candidates {
            let p = Path::new(path);
            if p.exists() {
                return Self::load(p);
            }
        }

        // No config found — return defaults
        Ok(Self {
            agent: AgentSection::default(),
            providers: HashMap::new(),
            intents: HashMap::new(),
            telegram: None,
            shepherd: None,
            identity: None,
            soullog: None,
            logging: LoggingConfig::default(),
            supervisor: SupervisorConfig::default(),
        })
    }
}

fn dirs_candidate() -> String {
    if let Some(home) = std::env::var_os("HOME") {
        format!("{}/.config/nexus/agent.toml", home.to_string_lossy())
    } else {
        "~/.config/nexus/agent.toml".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_toml() {
        let toml_str = r#"
[agent]
strategy = "round_robin"
max_turns = 30

[providers.groq]
kind = "openai"
api_key_env = "GROQ_API_KEY"
base_url = "https://api.groq.com/openai/v1"
model = "llama-3.3-70b-versatile"
rpm = 30
weight = 30

[intents.reasoning]
preferred = ["groq"]
"#;
        let config: AgentToml = toml::from_str(toml_str).unwrap();
        assert_eq!(config.agent.strategy, "round_robin");
        assert_eq!(config.agent.max_turns, 30);
        assert!(config.providers.contains_key("groq"));
        assert_eq!(config.providers["groq"].model, "llama-3.3-70b-versatile");
        assert_eq!(config.providers["groq"].rpm, Some(30));
        assert!(config.intents.contains_key("reasoning"));
    }

    #[test]
    fn defaults_when_empty() {
        let toml_str = "";
        let config: AgentToml = toml::from_str(toml_str).unwrap();
        assert_eq!(config.agent.strategy, "failover");
        assert_eq!(config.agent.max_turns, 50);
        assert_eq!(config.agent.context_strategy, "rlm");
        assert!(config.providers.is_empty());
    }

    #[test]
    fn context_strategy_from_toml() {
        let toml_str = r#"
[agent]
context_strategy = "semantic_graph"
"#;
        let config: AgentToml = toml::from_str(toml_str).unwrap();
        assert_eq!(config.agent.context_strategy, "semantic_graph");
    }

    #[test]
    fn context_strategy_classic() {
        let toml_str = r#"
[agent]
context_strategy = "classic"
"#;
        let config: AgentToml = toml::from_str(toml_str).unwrap();
        assert_eq!(config.agent.context_strategy, "classic");
    }

    #[test]
    fn telegram_config_from_toml() {
        let toml_str = r#"
[telegram]
token = "123:ABC"
allowed_users = ["78131249", "@Gonzih"]
startup_chat_id = "78131249"
"#;
        let config: AgentToml = toml::from_str(toml_str).unwrap();
        let tg = config.telegram.unwrap();
        assert_eq!(tg.token, Some("123:ABC".into()));
        assert_eq!(tg.allowed_users.len(), 2);
        assert_eq!(tg.startup_chat_id, Some("78131249".into()));
    }

    #[test]
    fn logging_config_defaults() {
        let toml_str = "";
        let config: AgentToml = toml::from_str(toml_str).unwrap();
        assert_eq!(config.logging.dir, ".nexus-logs");
        assert!(config.logging.events);
        assert!(!config.logging.conversations);
    }

    #[test]
    fn supervisor_config_defaults() {
        let toml_str = "";
        let config: AgentToml = toml::from_str(toml_str).unwrap();
        assert!(!config.supervisor.enabled);
        assert!(!config.supervisor.self_compile);
    }

    #[test]
    fn shepherd_config_from_toml() {
        let toml_str = r#"
[shepherd]
url = "ws://localhost:8084/ws/sessions/test_123"
heartbeat_secs = 30
"#;
        let config: AgentToml = toml::from_str(toml_str).unwrap();
        let shep = config.shepherd.unwrap();
        assert_eq!(
            shep.url,
            Some("ws://localhost:8084/ws/sessions/test_123".into())
        );
        assert_eq!(shep.heartbeat_secs, Some(30));
    }

    #[test]
    fn shepherd_config_absent() {
        let toml_str = "";
        let config: AgentToml = toml::from_str(toml_str).unwrap();
        assert!(config.shepherd.is_none());
    }

    #[test]
    fn identity_config_from_toml() {
        let toml_str = r#"
[identity]
id_service_url = "http://id:8080"
name = "my_agent"
key_dir = "/tmp/keys"
"#;
        let config: AgentToml = toml::from_str(toml_str).unwrap();
        let id = config.identity.unwrap();
        assert_eq!(id.id_service_url, "http://id:8080");
        assert_eq!(id.name, Some("my_agent".into()));
        assert_eq!(id.key_dir, Some("/tmp/keys".into()));
    }

    #[test]
    fn identity_config_defaults() {
        let toml_str = r#"
[identity]
"#;
        let config: AgentToml = toml::from_str(toml_str).unwrap();
        let id = config.identity.unwrap();
        assert_eq!(id.id_service_url, "http://localhost:8080");
        assert!(id.name.is_none());
        assert!(id.key_dir.is_none());
    }

    #[test]
    fn soullog_config_from_toml() {
        let toml_str = r#"
[soullog]
url = "http://soullog:8086"
service = "custom-agent"
batch_size = 50
flush_interval_ms = 500
"#;
        let config: AgentToml = toml::from_str(toml_str).unwrap();
        let sl = config.soullog.unwrap();
        assert_eq!(sl.url, "http://soullog:8086");
        assert_eq!(sl.service, "custom-agent");
        assert_eq!(sl.batch_size, 50);
        assert_eq!(sl.flush_interval_ms, 500);
    }

    #[test]
    fn soullog_config_defaults() {
        let toml_str = r#"
[soullog]
"#;
        let config: AgentToml = toml::from_str(toml_str).unwrap();
        let sl = config.soullog.unwrap();
        assert_eq!(sl.url, "http://localhost:8086");
        assert_eq!(sl.service, "nexus-agent");
        assert_eq!(sl.batch_size, 100);
        assert_eq!(sl.flush_interval_ms, 1000);
    }

    #[test]
    fn identity_and_soullog_absent() {
        let toml_str = "";
        let config: AgentToml = toml::from_str(toml_str).unwrap();
        assert!(config.identity.is_none());
        assert!(config.soullog.is_none());
    }

    #[test]
    fn multiple_providers() {
        let toml_str = r#"
[providers.gemini]
kind = "openai"
api_key_env = "GEMINI_API_KEY"
base_url = "https://generativelanguage.googleapis.com/v1beta/openai"
model = "gemini-2.0-flash"
context_window = 1048576
rpm = 15
rpd = 1500
tpm = 1000000

[providers.groq]
kind = "openai"
api_key_env = "GROQ_API_KEY"
base_url = "https://api.groq.com/openai/v1"
model = "llama-3.3-70b-versatile"
rpm = 30

[providers.ollama]
kind = "ollama"
base_url = "http://localhost:11434"
model = "qwen2.5-coder:32b"
context_window = 32768

[intents.reasoning]
preferred = ["gemini"]
models = { gemini = "gemini-1.5-pro" }

[intents.code_generation]
preferred = ["groq", "gemini"]

[intents.quick_chat]
preferred = ["groq"]
models = { groq = "llama-3.1-8b-instant" }
"#;
        let config: AgentToml = toml::from_str(toml_str).unwrap();
        assert_eq!(config.providers.len(), 3);
        assert_eq!(config.intents.len(), 3);
        assert_eq!(config.providers["gemini"].context_window, 1_048_576);
        assert_eq!(config.providers["gemini"].tpm, Some(1_000_000));
        assert_eq!(config.providers["ollama"].kind, "ollama");
    }
}

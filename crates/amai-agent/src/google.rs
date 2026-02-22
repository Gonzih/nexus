use std::sync::Arc;

use soul_core::tool::ToolRegistry;
use soul_google_tools::{GoogleAuth, GoogleCredentials, GoogleTokenAuth, GoogleTokens};

/// Default credentials path: ~/.amai/google/credentials.json
fn default_credentials_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".amai/google/credentials.json"))
}

/// Default tokens path: ~/.amai/google/tokens/default.json
fn default_tokens_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".amai/google/tokens/default.json"))
}

/// Load Google auth from filesystem if both credentials and tokens exist.
/// Returns None if not configured — agent works fine without Google tools.
pub async fn load_google_auth(
    creds_override: Option<&str>,
    tokens_override: Option<&str>,
) -> Option<Arc<dyn GoogleAuth>> {
    let creds_path = creds_override
        .map(std::path::PathBuf::from)
        .or_else(default_credentials_path)?;

    let tokens_path = tokens_override
        .map(std::path::PathBuf::from)
        .or_else(default_tokens_path)?;

    if !creds_path.exists() || !tokens_path.exists() {
        return None;
    }

    let creds_data = tokio::fs::read_to_string(&creds_path).await.ok()?;
    let tokens_data = tokio::fs::read_to_string(&tokens_path).await.ok()?;

    let credentials: GoogleCredentials = serde_json::from_str(&creds_data).ok()?;
    let tokens: GoogleTokens = serde_json::from_str(&tokens_data).ok()?;

    Some(Arc::new(GoogleTokenAuth::new(credentials, tokens)))
}

/// Register all Google tools into the agent's tool registry.
pub fn register_google_tools(registry: &mut ToolRegistry, auth: Arc<dyn GoogleAuth>) {
    for tool in soul_google_tools::all_tools(auth) {
        registry.register(tool);
    }
}

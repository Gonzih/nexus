use async_trait::async_trait;
use serde_json::json;
use soul_core::tool::{Tool, ToolOutput};
use soul_core::ToolDefinition;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Tool that fetches content from a URL via the backend proxy (CORS bypass).
pub struct WebFetchTool {
    proxy_base_url: String,
}

impl WebFetchTool {
    pub fn new(proxy_base_url: impl Into<String>) -> Self {
        Self {
            proxy_base_url: proxy_base_url.into(),
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_fetch".into(),
            description: "Fetch content from a URL. The request is proxied through the backend to bypass CORS restrictions. Returns the page content as text.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        arguments: serde_json::Value,
        _partial_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> soul_core::error::SoulResult<ToolOutput> {
        let url = arguments
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if url.is_empty() {
            return Ok(ToolOutput::error("No URL provided"));
        }

        debug!(url = %url, "Fetching URL via proxy");

        let proxy_url = format!("{}/proxy/fetch?url={}", self.proxy_base_url, urlencoding(&url));

        let client = reqwest::Client::new();
        match client.get(&proxy_url).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    match resp.text().await {
                        Ok(body) => {
                            debug!(url = %url, body_len = body.len(), "Fetch succeeded");
                            // Truncate to 50KB
                            let truncated = if body.len() > 51_200 {
                                format!(
                                    "{}\n\n[Truncated: showing first 50KB of {} bytes]",
                                    &body[..51_200],
                                    body.len()
                                )
                            } else {
                                body
                            };
                            Ok(ToolOutput::success(truncated))
                        }
                        Err(e) => {
                            warn!(url = %url, error = %e, "Failed to read response body");
                            Ok(ToolOutput::error(format!("Failed to read response: {e}")))
                        }
                    }
                } else {
                    let status = resp.status();
                    warn!(url = %url, status = %status, "Fetch failed with non-success status");
                    Ok(ToolOutput::error(format!(
                        "Fetch failed with status {status}"
                    )))
                }
            }
            Err(e) => {
                warn!(url = %url, error = %e, "Fetch request failed");
                Ok(ToolOutput::error(format!("Fetch request failed: {e}")))
            }
        }
    }
}

fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

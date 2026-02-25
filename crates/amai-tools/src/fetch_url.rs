use async_trait::async_trait;
use serde_json::json;
use soul_core::tool::{Tool, ToolOutput};
use soul_core::ToolDefinition;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Native HTTP fetch tool — fetches content from a URL directly (no proxy).
///
/// Unlike `WebFetchTool` (which proxies through the backend for CORS bypass),
/// this tool makes a direct reqwest call. Use in native/CLI agent contexts.
pub struct FetchUrlTool;

impl FetchUrlTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FetchUrlTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for FetchUrlTool {
    fn name(&self) -> &str {
        "fetch_url"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "fetch_url".into(),
            description: "Fetch the content of a URL directly. Returns the response body as text. \
                          Useful for reading documentation, API responses, web pages, or any HTTP endpoint. \
                          Truncates to 100KB. For large JSON APIs prefer http_request.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch (http or https)"
                    },
                    "headers": {
                        "type": "object",
                        "description": "Optional HTTP headers to include (e.g. Accept, Authorization)",
                        "additionalProperties": { "type": "string" }
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

        debug!(url = %url, "fetch_url: fetching");

        let client = reqwest::Client::builder()
            .user_agent("amai-agent/1.0 (research agent)")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| soul_core::error::SoulError::Provider(e.to_string()))?;

        let mut req = client.get(&url);

        // Apply optional headers
        if let Some(headers) = arguments.get("headers").and_then(|v| v.as_object()) {
            for (key, val) in headers {
                if let Some(val_str) = val.as_str() {
                    req = req.header(key.as_str(), val_str);
                }
            }
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.text().await {
                        Ok(body) => {
                            debug!(url = %url, body_len = body.len(), "fetch_url: success");
                            const MAX_BYTES: usize = 100 * 1024; // 100KB
                            let output = if body.len() > MAX_BYTES {
                                format!(
                                    "{}\n\n[Truncated: showing first 100KB of {} total bytes]",
                                    &body[..MAX_BYTES],
                                    body.len()
                                )
                            } else {
                                body
                            };
                            Ok(ToolOutput::success(output))
                        }
                        Err(e) => {
                            warn!(url = %url, error = %e, "fetch_url: body read failed");
                            Ok(ToolOutput::error(format!("Failed to read response body: {e}")))
                        }
                    }
                } else {
                    let body = resp.text().await.unwrap_or_default();
                    warn!(url = %url, status = %status, "fetch_url: non-success status");
                    Ok(ToolOutput::error(format!(
                        "HTTP {status}: {body}"
                    )))
                }
            }
            Err(e) => {
                warn!(url = %url, error = %e, "fetch_url: request failed");
                Ok(ToolOutput::error(format!("Request failed: {e}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_and_schema() {
        let t = FetchUrlTool::new();
        assert_eq!(t.name(), "fetch_url");
        let def = t.definition();
        assert_eq!(def.name, "fetch_url");
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("url")));
    }

    #[tokio::test]
    async fn empty_url_returns_error() {
        let t = FetchUrlTool::new();
        let out = t.execute("c1", json!({"url": ""}), None).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("No URL provided"));
    }

    #[tokio::test]
    async fn missing_url_returns_error() {
        let t = FetchUrlTool::new();
        let out = t.execute("c2", json!({}), None).await.unwrap();
        assert!(out.is_error);
    }
}
